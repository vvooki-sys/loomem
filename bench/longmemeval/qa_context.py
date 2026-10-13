"""Pack actual returned evidence with the frozen reader's real tokenizer.

The budget covers the HTML-escaped JSON evidence delivered by reader_messages.
`context` is the unescaped compact JSON input to that single escaping boundary.
Question, date,
wrappers, system instructions, output reservation and API framing are separate.
No estimate here claims to count undocumented transport/message overhead.
"""
import hashlib
import html
import importlib.metadata
import json
import re
from functools import lru_cache


@lru_cache(maxsize=8)
def _tokenizer(reader_model):
    if not isinstance(reader_model, str) or not re.search(r"-20\d{2}-\d{2}-\d{2}$", reader_model):
        raise ValueError("Reader must use an immutable dated model ID, not an alias")
    try:
        import tiktoken
    except ImportError as error:
        raise RuntimeError("QA requires tiktoken in the isolated evaluation environment") from error
    try:
        encoding = tiktoken.encoding_for_model(reader_model)
    except KeyError as error:
        raise ValueError("No known tokenizer for the frozen reader model") from error
    return encoding


def tokenizer_identity(reader_model):
    """Tokenizer identity; does not assert that the model exists at the API."""
    encoding = _tokenizer(reader_model)
    return {"library": "tiktoken", "version": importlib.metadata.version("tiktoken"),
            "encoding": encoding.name, "reader_model": reader_model,
            "special_tokens": "ordinary_text"}


def count_text(text, reader_model):
    """Count literal text, including Unicode and apparent special-token strings."""
    if not isinstance(text, str):
        raise ValueError("Token counting requires a string")
    return len(_tokenizer(reader_model).encode(text, disallowed_special=()))


def serialize_evidence(records):
    """The exact context representation; never parse or rebuild record.content."""
    return json.dumps(records, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def render_evidence(context):
    """Exact reader wire evidence, matching qa_prompts.reader_messages escaping."""
    return html.escape(context, quote=False)


def _validate_records(records):
    if not isinstance(records, list):
        raise ValueError("Evidence records must be a list")
    seen, validated = set(), []
    for record in records:
        if not isinstance(record, dict) or set(record) != {"record_id", "content"}:
            raise ValueError("Each record must contain exactly record_id/content")
        identity, content = record["record_id"], record["content"]
        if not isinstance(identity, str) or not identity or identity in seen:
            raise ValueError("Record IDs must be unique nonempty strings")
        if not isinstance(content, str):
            raise ValueError("Evidence content must be the returned string")
        seen.add(identity)
        validated.append({"record_id": identity, "content": content})
    return validated


def pack_context(records, token_budget, reader_model):
    """Rank-stable first-fit packing of complete records, without truncation.

    An oversized record is skipped and later smaller records may fit. Each
    candidate is counted as a FULL serialized, HTML-escaped array because BPE token counts
    are not additive. dropped_record_tokens measures each dropped record alone
    in a one-element array (not incremental tokens or a summed budget).
    An empty result is explicit, including when input exists but nothing fits.
    """
    if type(token_budget) is not int or token_budget <= 0:
        raise ValueError("Context token budget must be a positive integer")
    records = _validate_records(records)
    context = serialize_evidence([])
    context_tokens = count_text(render_evidence(context), reader_model)
    if context_tokens > token_budget:
        raise ValueError("Budget cannot fit the empty serialized evidence array")
    selected, dropped, dropped_tokens = [], [], {}
    for record in records:
        candidate = serialize_evidence(selected + [record])
        tokens = count_text(render_evidence(candidate), reader_model)
        if tokens <= token_budget:
            selected.append(record)
            context, context_tokens = candidate, tokens
        else:
            dropped.append(record["record_id"])
            dropped_tokens[record["record_id"]] = count_text(render_evidence(serialize_evidence([record])), reader_model)
    return {"schema_version": 1, "packing_policy": "ranked_full_records_first_fit_v1",
            "context": context, "context_tokens": context_tokens,
            "context_sha256": hashlib.sha256(context.encode("utf-8")).hexdigest(),
            "rendered_context_sha256": hashlib.sha256(render_evidence(context).encode("utf-8")).hexdigest(),
            "serialization": "compact_json_then_html_escape_quote_false",
            "context_token_budget": token_budget, "budget_scope": "rendered_evidence_only",
            "selected_ids": [record["record_id"] for record in selected],
            "dropped_ids": dropped, "dropped_record_tokens": dropped_tokens,
            "empty": not selected, "tokenizer": tokenizer_identity(reader_model)}
