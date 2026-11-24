"""The pilot's first paid step: one minimal control call to the judge model (owner 2026-11-22).

A fixed short probe is sent to the ``control`` profile (judge model,
``reasoning_effort=high``, ``max_completion_tokens=4096``) through the same
``paid_completion`` path as every other paid call, letter ``R``. The
reservation is ``sheet.control_bound()`` under the campaign's policy and must
not exceed 0.24 USD, otherwise the call is refused before anything is
reserved. A provider rejection (HTTP 400, ``Rejected4xx``) propagates: that is
the pilot stop the protocol asks for. Nothing here is retried automatically
(only the owner's ``run --retry-rejected`` sends a rejected control request again).
"""
from decimal import Decimal
import time

from paid_call_journal import canonical_bytes, sha256_hex
from paid_campaign import CampaignError
from paid_openai_wire import paid_completion
from paid_price_sheet import CONTROL_STAGE

LETTER = "K"                         # its own letter and sublimit (owner/coordinator 2026-11-23)
RESERVE_CAP_USD = Decimal("0.2374")  # the control reservation and the K sublimit may not exceed this
SYSTEM = "Reply with the single word OK."
USER = "OK?"


def control_messages():
    return [{"role": "system", "content": SYSTEM}, {"role": "user", "content": USER}]


def control_binding(campaign):
    messages = control_messages()
    return {"history_id": None, "session_id": None, "question_id": None, "arm": None,
            "stage": CONTROL_STAGE, "attempt": 1,
            "manifest_sha256": campaign.spec["holdout"]["manifest_sha256"],
            "context_sha256": sha256_hex(canonical_bytes(messages)),
            "prompt_sha256": sha256_hex(SYSTEM.encode("utf-8"))}


def control_reservation(campaign, sheet):
    """The exact maximum this call reserves; refused above RESERVE_CAP_USD."""
    bound = sheet.control_bound()
    max_usd = sheet.max_usd(bound, campaign.policy)
    if max_usd > RESERVE_CAP_USD:
        raise CampaignError(f"control call would reserve {max_usd} USD under policy {campaign.policy}, "
                            f"above the pilot cap {RESERVE_CAP_USD}")
    if campaign.sublimits[LETTER] > RESERVE_CAP_USD:
        raise CampaignError(f"sublimit {LETTER} is {campaign.sublimits[LETTER]} USD; the control letter may not "
                            f"exceed {RESERVE_CAP_USD}")
    return bound, max_usd


def control_requests(journal_state):
    """request_id -> intent for every control-stage intent in the journal."""
    return {rid: intent for rid, intent in journal_state.intents.items()
            if intent.get("stage") == CONTROL_STAGE}


def has_settled_control(journal_state):
    """True when a control request settled with an answer (ok or length)."""
    return any(journal_state.settled.get(rid, {}).get("outcome") in ("ok", "length")
               for rid in control_requests(journal_state))


def control_call(campaign, journal, sheet, transport, *, precount=None, acknowledged=frozenset(),
                 clock=time.time, retry_rejected=frozenset()):
    if not campaign.has_control_profile():
        raise CampaignError(f"campaign has no {CONTROL_STAGE!r} profile: the pilot control call is refused")
    bound, _ = control_reservation(campaign, sheet)
    if hasattr(precount, "set_context"):
        precount.set_context(stage=CONTROL_STAGE, arm=None, question_id=None, history_id=None,
                             session_id=None, r_tokens=bound.r_tokens)
    return paid_completion(campaign, journal, sheet, letter=LETTER, stage=CONTROL_STAGE,
                           binding=control_binding(campaign), messages=control_messages(),
                           response_format=None, transport=transport, bound=bound,
                           acknowledged=acknowledged, clock=clock, precount=precount,
                           retry_rejected=retry_rejected)
