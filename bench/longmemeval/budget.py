"""Fail-closed, process-safe spend ledger for local paid benchmark runs.

Reserve the conservative maximum BEFORE issuing a paid request. Never settle an
ambiguous timeout: its full reservation remains committed across process restarts.
The ledger cannot validate a provider's price or upper bound; callers must do so.
POSIX local filesystem only; all participants must share this exact ledger path.
"""
from contextlib import contextmanager
from decimal import Decimal, InvalidOperation, localcontext
import fcntl
import json
import os
from pathlib import Path
import tempfile
import uuid


class BudgetError(RuntimeError):
    """The call must not proceed."""


class BudgetExceeded(BudgetError):
    pass


class BudgetBreach(BudgetError):
    """Actual usage exceeded a reservation; recorded and all new calls blocked."""


class DuplicateReservation(BudgetError):
    """An ID was already used, possibly by an uncertain paid request."""


def _money(value):
    if isinstance(value, bool) or not isinstance(value, (str, int, float, Decimal)):
        raise ValueError("money must be a finite nonnegative decimal")
    try:
        amount = Decimal(str(value))
    except InvalidOperation as exc:
        raise ValueError("invalid decimal money") from exc
    if not amount.is_finite() or amount < 0:
        raise ValueError("money must be finite and nonnegative")
    # Bound hostile exponents/precision without ever silently rounding money.
    if len(amount.as_tuple().digits) > 28 or not -12 <= amount.as_tuple().exponent <= 12:
        raise ValueError("money exceeds supported exact precision (12 decimal places)")
    return amount


def _totals(state):
    with localcontext() as context:
        context.prec = 64
        spent = sum((_money(r["actual_usd"]) for r in state["requests"].values()
                     if r["actual_usd"] is not None), Decimal(0))
        reserved = sum((_money(r["max_usd"]) for r in state["requests"].values()
                        if r["actual_usd"] is None), Decimal(0))
        limit = _money(state["limit_usd"])
        return {"limit_usd": limit, "spent_usd": spent, "reserved_usd": reserved,
                "committed_usd": spent + reserved, "remaining_usd": limit - spent - reserved,
                "request_count": len(state["requests"]),
                "pending_count": sum(r["actual_usd"] is None for r in state["requests"].values()),
                "breached": any(r["actual_usd"] is not None and
                                _money(r["actual_usd"]) > _money(r["max_usd"])
                                for r in state["requests"].values())}


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise BudgetError("duplicate ledger key")
        result[key] = value
    return result


class BudgetLedger:
    def __init__(self, path, limit_usd):
        self.path = Path(path).resolve()
        self.limit = _money(limit_usd)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.lock_path = self.path.with_name(self.path.name + ".lock")
        with self._locked() as lock:
            exists = self.path.exists()
            marked = os.fstat(lock.fileno()).st_size > 0
            if not exists and marked:
                raise BudgetError("initialized ledger missing; refusing to reset spending")
            if not marked:
                # Persist marker before initial state: crash in between fails closed.
                lock.write(b"initialized\n")
                lock.flush()
                os.fsync(lock.fileno())
            if not exists:
                self._write({"schema": 1, "limit_usd": str(self.limit), "requests": {}})
            self._read()

    @contextmanager
    def _locked(self):
        descriptor = os.open(self.lock_path, os.O_RDWR | os.O_CREAT, 0o600)
        with os.fdopen(descriptor, "r+b") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            try:
                yield lock
            finally:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)

    def _read(self):
        try:
            state = json.loads(self.path.read_text(), object_pairs_hook=_unique_object)
            if set(state) != {"schema", "limit_usd", "requests"} or type(state["schema"]) is not int or state["schema"] != 1:
                raise ValueError("unsupported ledger schema")
            if _money(state["limit_usd"]) != self.limit:
                raise ValueError("ledger budget cap is immutable")
            self._validate_requests(state["requests"])
            totals = _totals(state)
            if totals["committed_usd"] > self.limit and not totals["breached"]:
                raise ValueError("unexplained over-cap ledger")
            return state
        except (OSError, ValueError, TypeError, KeyError, AttributeError) as exc:
            raise BudgetError("unreadable, invalid or incompatible budget ledger") from exc

    @staticmethod
    def _validate_requests(requests):
        if not isinstance(requests, dict):
            raise ValueError("invalid request map")
        tokens = set()
        for request_id, row in requests.items():
            if not isinstance(request_id, str) or not request_id:
                raise ValueError("invalid request ID")
            if set(row) != {"token", "max_usd", "actual_usd"}:
                raise ValueError("invalid reservation")
            if not isinstance(row["token"], str) or not row["token"] or row["token"] in tokens:
                raise ValueError("invalid or duplicate reservation token")
            tokens.add(row["token"])
            _money(row["max_usd"])
            if row["actual_usd"] is not None:
                _money(row["actual_usd"])

    def _write(self, state):
        descriptor, temporary = tempfile.mkstemp(prefix=self.path.name + ".", dir=self.path.parent)
        try:
            with os.fdopen(descriptor, "w") as output:
                json.dump(state, output, sort_keys=True, allow_nan=False)
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, self.path)
            directory = os.open(self.path.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    def reserve(self, request_id, max_usd):
        """Durably reserve, returning a token only after data and directory fsync.

        Reusing request_id always fails, including after successful settlement.
        An I/O error must abort the paid call, even if reservation was persisted.
        """
        if not isinstance(request_id, str) or not request_id.strip():
            raise ValueError("request_id must be a nonempty string")
        maximum = _money(max_usd)
        with self._locked():
            state = self._read()
            if request_id in state["requests"]:
                raise DuplicateReservation("request ID already reserved; do not retry blindly")
            totals = _totals(state)
            if totals["breached"]:
                raise BudgetBreach("prior usage exceeded its reservation; new calls blocked")
            if maximum > totals["remaining_usd"]:
                raise BudgetExceeded("request maximum exceeds remaining budget")
            token = uuid.uuid4().hex
            state["requests"][request_id] = {"token": token, "max_usd": str(maximum), "actual_usd": None}
            self._write(state)
            return token

    def verify_reservation(self, token, request_id, max_usd):
        """Verify a resumed artifact belongs to this exact durable reservation.

        This is read-only and never authorizes another provider request. A cached
        completion must additionally settle its confirmed cost: a changed cost
        conflicts with an existing settlement and fails closed.
        """
        maximum = _money(max_usd)
        if not isinstance(request_id, str) or not request_id:
            raise BudgetError("invalid cached reservation request ID")
        with self._locked():
            state = self._read()
            row = state["requests"].get(request_id)
            if row is None or row["token"] != token or _money(row["max_usd"]) != maximum:
                raise BudgetError("cached artifact does not match its durable reservation")

    def settle(self, token, actual_usd):
        """Persist confirmed billed usage. Ambiguous/missing usage is not zero.

        Identical settlement is idempotent; a different settlement fails closed.
        Over-reservation usage is saved before raising BudgetBreach.
        """
        actual = _money(actual_usd)
        with self._locked():
            state = self._read()
            row = next((row for row in state["requests"].values() if row["token"] == token), None)
            if row is None:
                raise BudgetError("unknown reservation token")
            if row["actual_usd"] is not None and _money(row["actual_usd"]) != actual:
                raise BudgetError("reservation already settled with different usage")
            row["actual_usd"] = str(actual)
            self._write(state)
            if actual > _money(row["max_usd"]):
                raise BudgetBreach("actual usage exceeded reserved maximum; recorded and frozen")

    def totals(self):
        """Read exact Decimal totals under the same cross-process lock."""
        with self._locked():
            return _totals(self._read())
