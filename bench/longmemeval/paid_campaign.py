"""Campaign registry: one explicit limit, per-letter sublimits, no reset (plan §2).

``campaign.json`` is written once (create-only) and never changed. Money lives
only in ``budget.BudgetLedger`` (``budget.json`` next to it): the ledger's
immutable limit must equal ``limit_usd`` of the registry, otherwise opening the
campaign is refused. Sublimits E/F/S/A/R are registry fields enforced from the
journal's committed sums before every reservation. A missing limit, a missing
sublimit, or a sublimit sum above the limit is a refusal, never a default.
"""
from contextlib import contextmanager
from decimal import Decimal, InvalidOperation
import json
import os
from pathlib import Path
import socket
import time

from budget import BudgetLedger
from paid_call_journal import LETTERS, canonical_bytes, sha256_hex, write_create_only

MODES = ("paid", "dry_run")
POLICIES = ("p1", "p2")
PROFILE_STAGES = ("extraction", "diagnostic", "audit", "selection", "answer", "judge", "review")
REASONING_EFFORTS = ("low", "medium", "high")
REQUIRED = ("schema", "mode", "created_at", "limit_usd", "sublimits_usd", "bound_policy",
            "p1_framing_accepted", "cost_sheet", "plan_json", "holdout", "profiles",
            "provider", "timeout_s", "judge_sees_sources")


class CampaignError(RuntimeError):
    """Refuse to create, open or continue the campaign."""


class SublimitExceeded(CampaignError):
    """A letter's committed sum plus this reservation would exceed its sublimit."""


def _money(value, label):
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise CampaignError(f"{label} must be a decimal string")
    try:
        amount = Decimal(str(value))
    except InvalidOperation as exc:
        raise CampaignError(f"{label} is not a decimal") from exc
    if not amount.is_finite() or amount < 0:
        raise CampaignError(f"{label} must be finite and nonnegative")
    return amount


def _pinned(section, label, keys):
    if not isinstance(section, dict) or not set(keys) <= set(section):
        raise CampaignError(f"{label} must carry {sorted(keys)}")
    for key in keys:
        value = section[key]
        if key == "stage_sha256":
            if not isinstance(value, dict) or any(not isinstance(v, str) or len(v) != 64 for v in value.values()):
                raise CampaignError(f"{label}.{key} must map stages to 64-hex SHA-256 values")
        elif key.endswith("sha256") and (not isinstance(value, str) or len(value) != 64):
            raise CampaignError(f"{label}.{key} must be a 64-hex SHA-256")


def _validate_profiles(profiles, sheet):
    if not isinstance(profiles, dict) or set(profiles) != set(PROFILE_STAGES):
        raise CampaignError(f"profiles must cover exactly {PROFILE_STAGES}")
    for stage, profile in profiles.items():
        if not isinstance(profile, dict) or set(profile) != {"model", "reasoning_effort", "max_completion_tokens"}:
            raise CampaignError(f"profile {stage!r} needs model, reasoning_effort, max_completion_tokens")
        if profile["reasoning_effort"] not in REASONING_EFFORTS:
            raise CampaignError(f"profile {stage!r}: reasoning_effort must be one of {REASONING_EFFORTS}")
        if type(profile["max_completion_tokens"]) is not int or profile["max_completion_tokens"] <= 0:
            raise CampaignError(f"profile {stage!r}: max_completion_tokens must be a positive int")
        if sheet is not None:
            sheet.require_priced(profile["model"])
            if profile["model"] != sheet.models()[stage]:
                raise CampaignError(f"profile {stage!r}: model {profile['model']!r} differs from the sheet")
            expected = sheet.output_bound(stage, profile["model"])
            if profile["max_completion_tokens"] != expected:
                raise CampaignError(f"profile {stage!r}: max_completion_tokens {profile['max_completion_tokens']} "
                                    f"differs from the sheet's O_i {expected}")


def validate_spec(spec, sheet=None):
    """Return the exact limit and sublimits; raise CampaignError on any missing or bad field."""
    if not isinstance(spec, dict):
        raise CampaignError("campaign spec must be an object")
    missing = [key for key in REQUIRED if key not in spec]
    if missing:
        raise CampaignError(f"campaign spec lacks {missing}")
    if spec["schema"] != 1:
        raise CampaignError("unsupported campaign schema")
    if spec["mode"] not in MODES:
        raise CampaignError(f"mode must be one of {MODES}")
    if spec["bound_policy"] not in POLICIES:
        raise CampaignError(f"bound_policy must be one of {POLICIES}")
    if spec["bound_policy"] == "p1" and spec["p1_framing_accepted"] is not True:
        raise CampaignError("P1 requires p1_framing_accepted: true (FRAMING/byte-BPE assumption)")
    limit = _money(spec["limit_usd"], "limit_usd")
    if limit <= 0:
        raise CampaignError("limit_usd must be positive")
    sub = spec["sublimits_usd"]
    if not isinstance(sub, dict) or set(sub) != set(LETTERS):
        raise CampaignError(f"sublimits_usd must name exactly {LETTERS}")
    sublimits = {letter: _money(sub[letter], f"sublimits_usd.{letter}") for letter in LETTERS}
    if sum(sublimits.values(), Decimal(0)) > limit:
        raise CampaignError("sum of sublimits exceeds limit_usd")
    _pinned(spec["cost_sheet"], "cost_sheet", ("path", "sha256"))
    _pinned(spec["plan_json"], "plan_json", ("path", "sha256", "stage_sha256"))
    _pinned(spec["holdout"], "holdout", ("dir", "manifest_sha256", "always50_sha256"))
    _pinned(spec["provider"], "provider", ("base_url", "api_key_env", "allowed_host"))
    if type(spec["timeout_s"]) not in (int, float) or spec["timeout_s"] <= 0:
        raise CampaignError("timeout_s must be positive")
    if spec["judge_sees_sources"] is not True:
        raise CampaignError("judge_sees_sources must be true for this protocol")
    _validate_profiles(spec["profiles"], sheet)
    return limit, sublimits


class Campaign:
    def __init__(self, directory, spec, ledger):
        self.directory = Path(directory)
        self.spec = spec
        self.ledger = ledger
        self.limit, self.sublimits = validate_spec(spec)

    @property
    def campaign_id(self):
        return self.spec["campaign_id"]

    @property
    def mode(self):
        return self.spec["mode"]

    @property
    def policy(self):
        return self.spec["bound_policy"]

    def profile(self, stage):
        return dict(self.spec["profiles"][stage])

    def require_paid_mode(self):
        if self.mode != "paid":
            raise CampaignError(f"campaign mode is {self.mode!r}: a paid run is refused")

    def check_sublimit(self, letter, committed, add):
        """committed = journal sum for the letter; add = this reservation's maximum."""
        if letter not in LETTERS:
            raise CampaignError(f"unknown letter {letter!r}")
        total = Decimal(committed) + Decimal(add)
        if total > self.sublimits[letter]:
            raise SublimitExceeded(f"letter {letter}: {total} would exceed sublimit {self.sublimits[letter]}")

    @contextmanager
    def run_lock(self):
        """Single-process guard: run.lock is created O_EXCL and never auto-removed on a crash."""
        lock = self.directory / "run.lock"
        try:
            descriptor = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError as exc:
            raise CampaignError(f"run.lock present ({lock}): another run owns this campaign; "
                                "remove it manually only after confirming no process is alive") from exc
        with os.fdopen(descriptor, "w") as handle:
            json.dump({"pid": os.getpid(), "started_at": time.time(), "host": socket.gethostname()}, handle)
            handle.flush()
            os.fsync(handle.fileno())
        try:
            yield lock
        finally:
            os.unlink(lock)


def create_campaign(directory, spec, sheet=None):
    """Create-only: an existing directory is a refusal (no reset of a campaign)."""
    directory = Path(directory)
    if "campaign_id" in spec:
        raise CampaignError("campaign_id is derived, not supplied")
    validate_spec(spec, sheet)
    try:
        directory.mkdir(parents=True, exist_ok=False)
    except FileExistsError as exc:
        raise CampaignError(f"campaign directory exists: {directory}") from exc
    full = {**spec, "campaign_id": sha256_hex(canonical_bytes(spec))}
    write_create_only(directory / "campaign.json", full)
    ledger = BudgetLedger(directory / "budget.json", str(Decimal(str(spec["limit_usd"]))))
    return Campaign(directory, full, ledger)


def open_campaign(directory, sheet=None):
    directory = Path(directory)
    path = directory / "campaign.json"
    try:
        spec = json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        raise CampaignError(f"campaign registry unreadable: {path}") from exc
    if not isinstance(spec, dict) or "campaign_id" not in spec:
        raise CampaignError("campaign registry lacks campaign_id")
    expected = sha256_hex(canonical_bytes({k: v for k, v in spec.items() if k != "campaign_id"}))
    if spec["campaign_id"] != expected:
        raise CampaignError("campaign registry was modified after creation")
    validate_spec(spec, sheet)
    try:
        ledger = BudgetLedger(directory / "budget.json", str(Decimal(str(spec["limit_usd"]))))
    except Exception as exc:  # BudgetError: missing-but-initialized ledger or a different limit
        raise CampaignError(f"budget ledger refused: {exc}") from exc
    return Campaign(directory, spec, ledger)
