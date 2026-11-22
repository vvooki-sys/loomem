"""Offline candidate for binding Rust Luna request to a provider wire.

No server, network, ledger or paid dispatch is provided. The existing frozen
broker remains unchanged. Its 65,536-byte cap on canonical body and wire and
2,000-token output cap are retained: larger requests are rejected, not
truncated. This function cannot enforce the HTTP server's separate raw-byte cap.
The default Rust cap of
2,048 therefore requires an explicit compatible configuration before use.
The legacy broker accepted only the Luna model; other models remain unsupported.
"""
import copy
import json

BODY_CAP = 65536
OUTPUT_CAP = 2000


def _canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False,
                      separators=(',', ':'), allow_nan=False).encode('utf-8')


def adapt(body):
    if type(body) is not dict:
        raise ValueError('engine_keys')
    legacy = set(body) == {'model', 'messages', 'max_tokens', 'temperature'}
    native = set(body) == {'model', 'messages', 'max_completion_tokens', 'reasoning_effort'}
    if not (legacy or native):
        raise ValueError('engine_keys')
    limit = body['max_tokens'] if legacy else body['max_completion_tokens']
    if body['model'] != 'gpt-6-luna' or type(limit) is not int or not 0 < limit <= OUTPUT_CAP:
        raise ValueError('model_or_cap')
    if legacy and (type(body['temperature']) not in (int, float) or body['temperature'] != 0):
        raise ValueError('temperature')
    if native and body['reasoning_effort'] != 'low':
        raise ValueError('reasoning_effort')
    messages = body['messages']
    if type(messages) is not list or len(messages) != 2:
        raise ValueError('messages')
    for message, role in zip(messages, ('system', 'user'), strict=True):
        if (type(message) is not dict or set(message) != {'role', 'content'}
                or message['role'] != role or type(message['content']) is not str):
            raise ValueError('message_shape')
    wire = {'model': body['model'], 'messages': copy.deepcopy(messages),
            'max_completion_tokens': limit, 'reasoning_effort': 'low',
            'service_tier': 'default', 'stream': False, 'store': False}
    if len(_canonical(body)) > BODY_CAP or len(_canonical(wire)) > BODY_CAP:
        raise ValueError('body_cap')
    return wire
