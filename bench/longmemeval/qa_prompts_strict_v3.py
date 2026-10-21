"""Opt-in bounded LongMemEval reader and schema-enum judge protocol.

The frozen strict-v2 module remains the source of the upstream judge rubrics
and reader user boundary. This module changes only the opt-in protocol.
"""

import html
import inspect

import qa_prompts as frozen
from validate import decode


PROTOCOL = "loomem-longmemeval-strict-v3-bounded"
BASE_PROTOCOL = frozen.PROTOCOL
PARSER_VERSION = "strict-json-label-v1"

UPSTREAM_COMMIT = frozen.UPSTREAM_COMMIT
UPSTREAM_EVALUATOR_SHA256 = frozen.UPSTREAM_EVALUATOR_SHA256
UPSTREAM_URL = frozen.UPSTREAM_URL
JUDGE_TEMPLATES = frozen.JUDGE_TEMPLATES
READER_USER_TEMPLATE = frozen.READER_USER_TEMPLATE

READER_SYSTEM = frozen.READER_SYSTEM + """
Put the answer first. Use at most 160 words, normally 1–4 sentences; if a list
is useful, use at most three short items. Do not quote, reproduce, or cite
evidence records, record IDs, evidence markers, or the evidence container. For
advice or preference questions, apply only supported personal facts and give
only the minimum useful recommendation; omit generic background and extra
alternatives. Preserve the existing missing-evidence rule and include every
explicitly requested part."""

JUDGE_SYSTEM = (
    "Return only the schema-constrained JSON object with label yes or no. "
    "Do not add any other key or explanation."
)

def reader_messages(question: str, question_date: str, evidence: str) -> list[dict[str, str]]:
    """Gold-blind reader boundary: question, date, and evidence only."""
    payload = READER_USER_TEMPLATE.format(
        question=html.escape(frozen._text(question, "question"), quote=False),
        question_date=html.escape(frozen._text(question_date, "question_date"), quote=False),
        evidence=html.escape(frozen._text(evidence, "evidence"), quote=False),
    )
    return [{"role": "system", "content": READER_SYSTEM}, {"role": "user", "content": payload}]


def judge_messages(
    question_type: str,
    question: str,
    answer: str | int | float,
    hypothesis: str,
    *,
    abstention: bool = False,
) -> list[dict[str, str]]:
    """Add an output-contract role while preserving the upstream rubric bytes."""
    upstream = frozen.judge_messages(
        question_type, question, answer, hypothesis, abstention=abstention
    )
    return [{"role": "system", "content": JUDGE_SYSTEM}, upstream[0]]


def judge_response_format() -> dict:
    """Construct the exact strict response-format payload without shared state."""
    return {
        "type": "json_schema",
        "json_schema": {
            "name": "longmemeval_judgment",
            "strict": True,
            "schema": {
                "type": "object",
                "properties": {"label": {"type": "string", "enum": ["yes", "no"]}},
                "required": ["label"],
                "additionalProperties": False,
            },
        },
    }


def parse_judge_label(text: str) -> bool:
    """Parse only the exact schema object; reject prose and coercions."""
    value = frozen._text(text, "judge response")
    try:
        payload = decode(value, "judge response")
    except ValueError as exc:
        raise ValueError("judge response must be strict label JSON") from exc
    if type(payload) is not dict or set(payload) != {"label"}:
        raise ValueError("judge response must contain only label")
    if type(payload["label"]) is not str or payload["label"] not in {"yes", "no"}:
        raise ValueError("judge label must be exactly yes or no")
    return payload["label"] == "yes"


is_abstention = frozen.is_abstention


def response_format_sha256() -> str:
    return frozen._digest(judge_response_format())


def reader_prompt_sha256() -> str:
    return frozen._digest(
        {
            "system": READER_SYSTEM,
            "user_template": READER_USER_TEMPLATE,
            "escaping": "html.escape(quote=False)",
            "roles": ["system", "user"],
        }
    )


def judge_prompt_sha256() -> str:
    return frozen._digest(
        {
            "system": JUDGE_SYSTEM,
            "templates": dict(JUDGE_TEMPLATES),
            "roles": ["system", "user"],
            "abstention": "_abs substring in question_id",
            "reference_answer": (
                "string unchanged; exact int or finite float str(); reject other types"
            ),
            "response_format": judge_response_format(),
            "parser": PARSER_VERSION,
        }
    )


def prompt_provenance() -> dict[str, str]:
    return {
        "protocol": PROTOCOL,
        "base_protocol": BASE_PROTOCOL,
        "upstream_commit": UPSTREAM_COMMIT,
        "upstream_evaluator_sha256": UPSTREAM_EVALUATOR_SHA256,
        "upstream_url": UPSTREAM_URL,
        "reader_prompt_sha256": reader_prompt_sha256(),
        "base_reader_prompt_sha256": frozen.reader_prompt_sha256(),
        "judge_prompt_sha256": judge_prompt_sha256(),
        "base_judge_prompt_sha256": frozen.judge_prompt_sha256(),
        "response_format_sha256": response_format_sha256(),
        "parser_version": PARSER_VERSION,
        "reader_parameters": ",".join(inspect.signature(reader_messages).parameters),
        "score_claim": "modified bounded evaluator; not an official benchmark score",
    }
