"""Hash-pinned access to the validation cost sheet (plan §9).

The sheet (``cost_sheet.py``, kept outside Git) is the only source of prices,
context windows, FRAMING, per-stage output bounds O_i and byte caps. It is
loaded with ``importlib`` from an explicit path whose SHA-256 must match the
campaign pin; nothing here duplicates a price. ``CACHED_INPUT`` and
``BATCH_PRICES`` are never read: the reservation is the no-cache maximum.

Bound policies (explicit at campaign creation):
  P1 ("p1"): R_i from ``bounds()`` (static bytes + 6 x dynamic caps + FRAMING),
       standard tier only (R_i <= long-context threshold), stop when
       ``usage.prompt_tokens > R_i``.
  P2 ("p2"): the full published context window at the matching price tier;
       refused (``PriceUnknown``) when the window or the tier price is None.
"""
from dataclasses import dataclass
from decimal import Decimal
import hashlib
import importlib.util
from pathlib import Path

POLICIES = ("p1", "p2")
STAGES = ("extraction", "diagnostic", "audit", "selection", "answer", "judge", "review")
CAP_NAMES = ("FACTS_JSON_CAP", "EVIDENCE_CAP", "DIAG_CAP", "ANSWER_CAP",
             "ORIGINALS_MENU_CAP", "QUESTION_CAP")
_REQUIRED = ("PRICES", "CONTEXT_WINDOW", "FRAMING", "OUT", "JUDGE_OUT", "JUDGE_MODEL",
             "MODEL", "SYSTEM", "E_CHUNK_OUT", "E_CHUNK_TOKENS", "cost", "bounds") + CAP_NAMES
# Owner decision 2026-11-22 (pilot protocol): the first paid step is one minimal
# control call to the judge model with reasoning_effort=high and this output cap;
# its input is a fixed short probe, bounded by CONTROL_STATIC_TOKENS + FRAMING.
CONTROL_STAGE = "control"
CONTROL_MAX_COMPLETION_TOKENS = 4096
CONTROL_STATIC_TOKENS = 1000


class SheetError(ValueError):
    """The sheet cannot be trusted; refuse any paid start."""


class PriceUnknown(SheetError):
    """No published price for this model/tier/window: refuse the paid call."""


@dataclass(frozen=True)
class StageBound:
    stage: str
    model: str
    r_tokens: int
    o_tokens: int


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _usd(tokens, price_per_million):
    # Prices are floats in the sheet; str() round-trips the written literal exactly.
    return Decimal(tokens) * Decimal(str(price_per_million)) / Decimal(1_000_000)


class Sheet:
    def __init__(self, module, path, sha256):
        self.module = module
        self.path = Path(path)
        self.sha256 = sha256

    # -- static tables -------------------------------------------------
    @property
    def prices(self):
        return self.module.PRICES

    @property
    def framing(self):
        return self.module.FRAMING

    def caps(self):
        return {name: getattr(self.module, name) for name in CAP_NAMES}

    def models(self):
        return dict(self.module.MODEL)

    def require_priced(self, model):
        if model not in self.module.PRICES:
            raise PriceUnknown(f"model {model!r} has no price in the sheet")
        return self.module.PRICES[model]

    def standard_tier_limit(self, model):
        return self.require_priced(model)[2]

    def context_window(self, model):
        self.require_priced(model)
        return self.module.CONTEXT_WINDOW.get(model)

    # -- bounds --------------------------------------------------------
    def output_bound(self, stage, model):
        """O_i: JUDGE_OUT for the judge model's stages, otherwise OUT (mirrors scope()).

        Extraction is the engine's chunked call: its cap is the sheet's
        ``E_CHUNK_OUT`` (the shipped ``[knowledge_extraction] max_tokens``), not
        the legacy single-call ``OUT["extraction"]`` (owner decision 2026-11-22).
        """
        if stage not in STAGES:
            raise SheetError(f"unknown stage {stage!r}")
        if stage == "extraction":
            return int(self.module.E_CHUNK_OUT)
        if model == self.module.JUDGE_MODEL and stage in self.module.JUDGE_OUT:
            return int(self.module.JUDGE_OUT[stage])
        return int(self.module.OUT[stage])

    def extraction_chunk_chars(self):
        """Engine chunker width in characters: input_chunk_tokens x 4 (chunking.rs)."""
        return int(self.module.E_CHUNK_TOKENS) * 4

    def control_bound(self):
        """The judge-model control call: fixed short probe, O = 4096 (pilot protocol)."""
        model = self.module.JUDGE_MODEL
        self.require_priced(model)
        return StageBound(CONTROL_STAGE, model, CONTROL_STATIC_TOKENS + int(self.module.FRAMING),
                          CONTROL_MAX_COMPLETION_TOKENS)

    def stage_profile(self, stage, sources_bytes_max, judge_sees_sources):
        if stage == "extraction":
            raise SheetError("extraction bound is per session: use extraction_bound()")
        rq = self.module.bounds(sources_bytes_max, judge_sees_sources)
        if stage not in rq:
            raise SheetError(f"unknown stage {stage!r}")
        model = self.module.MODEL[stage]
        self.require_priced(model)
        return StageBound(stage, model, int(rq[stage]), self.output_bound(stage, model))

    def extraction_bound(self, session_bytes):
        """Per extraction request: R = SYSTEM + bytes of the chunk the engine sends + FRAMING,
        O = E_CHUNK_OUT. One session may yield several chunk (and split-retry) requests;
        each is bounded and reserved on its own."""
        if type(session_bytes) is not int or session_bytes < 0:
            raise SheetError("session_bytes must be a nonnegative int")
        model = self.module.MODEL["extraction"]
        self.require_priced(model)
        r = int(self.module.SYSTEM["extraction"]) + session_bytes + int(self.module.FRAMING)
        return StageBound("extraction", model, r, self.output_bound("extraction", model))

    # -- money ---------------------------------------------------------
    def max_usd(self, bound, policy):
        """Exact Decimal reservation for one call under the chosen policy."""
        if policy not in POLICIES:
            raise SheetError(f"unknown bound policy {policy!r}")
        pin, pout, threshold, long_in, long_out = self.require_priced(bound.model)
        if policy == "p1":
            r = bound.r_tokens
            if r > threshold:
                raise PriceUnknown(f"P1 requires R_i <= {threshold} for {bound.model}; got {r}")
        else:
            r = self.context_window(bound.model)
            if r is None:
                raise PriceUnknown(f"context window of {bound.model} is not published")
        if r > threshold:
            if long_in is None or long_out is None:
                raise PriceUnknown(f"long-context price of {bound.model} is not published")
            pin, pout = long_in, long_out
        return _usd(r, pin) + _usd(bound.o_tokens, pout)

    def float_cost(self, model, r, o):
        """The sheet's own float arithmetic (reporting only; reservations use max_usd)."""
        return self.module.cost(model, r, o)


def load_sheet(path, expected_sha256):
    """Import the sheet from an explicit path only when its SHA-256 matches."""
    path = Path(path)
    if not isinstance(expected_sha256, str) or len(expected_sha256) != 64:
        raise SheetError("expected_sha256 must be a 64-hex SHA-256")
    if not path.is_file():
        raise SheetError(f"cost sheet not found: {path}")
    actual = sha256_file(path)
    if actual != expected_sha256:
        raise SheetError(f"cost sheet SHA-256 mismatch: expected {expected_sha256}, got {actual}")
    spec = importlib.util.spec_from_file_location("paid_cost_sheet_" + actual[:12], path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    missing = [name for name in _REQUIRED if not hasattr(module, name)]
    if missing:
        raise SheetError(f"cost sheet lacks required names: {missing}")
    for model, row in module.PRICES.items():
        if not (isinstance(row, tuple) and len(row) == 5):
            raise SheetError(f"PRICES[{model!r}] must be a 5-tuple")
    return Sheet(module, path, actual)
