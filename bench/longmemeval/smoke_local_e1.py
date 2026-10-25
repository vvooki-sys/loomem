#!/usr/bin/env python3
"""Synthetic E0/E1 integration smoke with a real local engine/model; no QA/API.

macOS sandbox-exec is required: the server cannot connect outside loopback.
All database/process state is temporary. Only synthetic reports are retained.
"""
import argparse
from contextlib import contextmanager
import json
from pathlib import Path
import platform
import shlex
import socket
import tempfile
import tomllib

from evidence_v2 import from_e0_turn_json
from qa_context_v2 import pack_context_v2
from representations import build_records
from runner import trace_projection
from runner_local import LocalEngineAdapter, effective_config
from runner_local_e1 import LocalEngineAdapterE1, effective_config_e1, session_records
from runner_local_v2 import trace_row_v2, write_trace_row_v2
from stage_diagnostics import diagnose
from validate import canonical_hash, sha256

SANDBOX = '/usr/bin/sandbox-exec'
PROFILE = ('(version 1)(allow default)(deny network-outbound)'
           '(allow network-outbound (remote ip "localhost:*"))')
READER = 'gpt-4.1-2025-04-14'  # Tokenizer identity only: no reader is called.


def workload():
    """Late information lies beyond one embedding window of the full source."""
    turns = []
    for i in range(4):
        turns += [{'role': 'user', 'content': (
            f'Notebook section {i}. ' + ('The workshop reviewed ordinary paper charts, '
            'wooden shelves, spare brushes, blue pencils, recycled packaging, '
            'small glass jars, cabinet hinges, measuring tapes and garden tools. ' * 6))},
                  {'role': 'assistant', 'content': f'Section {i} was noted for the workshop.'}]
    turns += [{'role': 'user', 'content': 'The saffron lantern rendezvous code is café-orbit.'},
              {'role': 'assistant', 'content': 'The saffron lantern rendezvous code was recorded.'}]
    return {'question_id': 'synthetic-local-smoke', 'question': 'What is the saffron lantern rendezvous code?',
            'haystack_session_ids': ['synthetic-session'],
            'haystack_dates': ['2023/05/20 (Sat) 02:21'], 'haystack_sessions': [turns]}


@contextmanager
def loopback_only():
    """Block Python DNS/connect paths outside loopback, including tokenizer downloads."""
    original = socket.getaddrinfo

    def checked(host, *args, **kwargs):
        if host not in ('127.0.0.1', 'localhost', '::1', None):
            raise OSError('Smoke forbids non-loopback networking')
        return original(host, *args, **kwargs)

    socket.getaddrinfo = checked
    try:
        yield
    finally:
        socket.getaddrinfo = original


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def settings_for(binary, template, model, root):
    """Bind the actual files the unmodified owned-engine adapter verifies."""
    return {'endpoint': f'http://127.0.0.1:{free_port()}', 'data_dir': str(root),
            'instance_id': 'synthetic-local-smoke', 'run_id': 'synthetic-local-smoke',
            'capture_context': True, 'top_k': 60, '_e1': {
                'provenance_role': 'claim', 'token_counter': 'embedder'},
            'local_engine': {'binary': str(binary), 'binary_sha256': sha256(binary),
                'template': str(template), 'template_sha256': sha256(template),
                'model_dir': str(model), 'model_onnx_sha256': sha256(model / 'model.onnx'),
                'tokenizer_sha256': sha256(model / 'tokenizer.json')}}


def checked_config(settings, e1, root):
    """Assert the effective config, not a claim about the template defaults."""
    text = (effective_config_e1 if e1 else effective_config)(settings, root)
    config = tomllib.loads(text)
    assert config['llm']['embedding_provider'] == 'local'
    assert config['llm']['embedding_dim'] == 384
    assert config['llm']['api_key'] == ''
    assert config['llm']['api_key_env'] == 'BENCH_NO_API_KEY'
    for section in ('knowledge_extraction', 'conversation_extraction', 'scheduler',
                    'entity_extraction', 'memory_generator', 'dream', 'profile',
                    'contradiction', 'associator'):
        assert config[section]['enabled'] is False, section
    assert config['search']['rerank_enabled'] is False
    assert config['search']['multi_query_enabled'] is False
    assert config['server']['host'] == '127.0.0.1'
    if e1:
        assert config['fragments']['enabled'] is True
        assert config['fragments']['token_counter'] == 'embedder'
        assert config['search']['evidence_fields'] is True
    else:
        assert config['fragments']['enabled'] is False
    return canonical_hash(config)


def assert_wire_trace(found):
    ids = [hit['id'] for hit in found['results']]
    evidence = trace_projection(found)
    assert evidence['coverage'] == 'post_rank_to_response'
    assert evidence['stages'][-1]['chunk_ids'] == ids
    assert sum(stage['stage'] == 'response' for stage in evidence['stages']) == 1
    assert not any(item['chunk_id'] in ids for item in
                   found['trace_metadata'].get('rejections', []))
    return ids


def inspect_e1_records(adapter, records, source):
    """Check metadata and byte ranges against the exact stored source bytes."""
    fragments = [record for record in records if ':f' in record['record_id']]
    assert fragments, 'Real search must return fragments'
    raw = source['content'].encode()
    for fragment in fragments:
        assert fragment['author'] in ('user', 'assistant')
        assert fragment['conversation_date'] == '2023-05-20'
        assert fragment['event_date'] is None
        assert fragment['event_date_provenance'] is None
        assert adapter.mapping[fragment['source_id']] == source['record_id']
        start, end = fragment['byte_start'], fragment['byte_end']
        assert 0 <= start < end <= len(raw)
        assert raw[start:end].decode() == fragment['content']
        assert adapter.map_id(adapter.reverse_map_id(fragment['record_id'])) == fragment['record_id']
    late = [record for record in fragments if 'café-orbit' in record['content']]
    assert late and late[0]['author'] == 'user', 'Late user information must reach reader context'
    head = adapter.fetch_records_v2([source['record_id'] + ':f0', source['record_id'] + ':f1'])
    assert [record['author'] for record in head] == ['user', 'assistant']
    fetched = adapter.fetch_records_v2([late[0]['record_id']])
    assert fetched == late, 'Neighbor REST read must preserve exact evidence'
    return late[0]['record_id']


def run_arm(settings, e1, row, output, binding):
    adapter = LocalEngineAdapterE1(settings) if e1 else LocalEngineAdapter(settings)
    records = session_records(row) if e1 else build_records(row, 'raw')
    expected = [record['record_id'] for record in records]
    arm = 'E1' if e1 else 'E0'
    try:
        adapter.identity()
        actual_work = adapter.invocation / str(adapter.counter)
        config_text = (actual_work / 'config.toml').read_text()
        config_hash = canonical_hash(tomllib.loads(config_text))
        assert config_hash == checked_config(settings, e1, actual_work)
        (output / f'{arm}-effective-config.toml').write_text(config_text)
        print(json.dumps({'arm': arm, 'stage': 'ingest'}), flush=True)
        ingested = adapter.request('/ingest', {'records': records})
        assert ingested['accepted_ids'] == expected
        print(json.dumps({'arm': arm, 'stage': 'readiness'}), flush=True)
        ready = adapter.request('/ready', {'expected_ids': expected})
        assert ready['ready'] and ready['indexed_ids'] == expected
        payload = {'query': row['question'], 'top_k': 5, 'trace': True,
                   'capture_context': True}
        print(json.dumps({'arm': arm, 'stage': 'retrieval'}), flush=True)
        found = adapter.request('/search', payload)
        (output / f'{arm}-wire-stages.json').write_text(json.dumps({
            'retrieved_ids': found['retrieved_ids'],
            'evidence': found.get('trace_metadata', {}).get('evidence')}, indent=2) + '\n')
        assert found['retrieved_ids']
        if e1:
            contexts = found['retrieved_records_v2']
            late_id = inspect_e1_records(adapter, contexts, records[0])
            sidecar = trace_row_v2(found, row, binding)
        else:
            contexts = [from_e0_turn_json(hit['content'], hit['record_id'])
                        for hit in found['retrieved_records']]
            late = [record for record in contexts if 'café-orbit' in record['content']]
            assert late and late[0]['author'] == 'user'
            assert late[0]['conversation_date'] == '2023-05-20'
            assert late[0]['source_id'] is None and late[0]['event_date'] is None
            late_id = late[0]['record_id']
            sidecar = trace_row_v2(found, row, binding)
        assert sidecar['evidence']['stages'][-1]['chunk_ids'] == found['retrieved_ids']
        packed = pack_context_v2(contexts, 8000, READER)
        assert late_id in packed['selected_ids']
        with (output / f'{arm}-trace.jsonl').open('x') as handle:
            write_trace_row_v2(handle, sidecar)
        report = diagnose([sidecar], {row['question_id']: [late_id]},
                          {row['question_id']: packed})
        checks = {'effective_config_sha256': config_hash, 'ingest_readiness': True, 'late_information_packed': True,
                  'trace_response_matches': True, 'stage_diagnostics': report,
                  'rejections_captured': len(sidecar['rejections']),
                  'retrieved_count': len(contexts), 'context_tokens': packed['context_tokens']}
        if e1:
            wire = adapter.engine_request('/v1/search', {
                'query': row['question'], 'top_k': 60, 'trace': True, 'dry_run': True})
            assert_wire_trace(wire)
            ranged = adapter.engine_request('/v1/search', {
                'query': row['question'], 'top_k': 60, 'trace': True, 'dry_run': True,
                'date_from': '2023-05-01', 'date_to': '2023-05-31'})
            assert_wire_trace(ranged)
            assert ranged['results'] == [], 'Conversation date is not an event date'
            source_id = adapter.reverse_map_id(records[0]['record_id'])
            deleted = adapter.tool_call('memory_delete', {'chunk_id': source_id})
            assert not deleted.get('isError', False)
            after = adapter.engine_request('/v1/search', {
                'query': row['question'], 'top_k': 60, 'trace': True, 'dry_run': True})
            assert_wire_trace(after)
            assert after['results'] == [], 'Deleting source must remove descendant retrieval'
            checks.update({'event_date_unknown_excluded': True, 'source_cascade': True,
                           'mcp_session_initialized': adapter.session_id is not None,
                           'source_fragment_id_mapping': True, 'metadata_byte_ranges': True})
        (output / f'{arm}-context.json').write_text(json.dumps(contexts, indent=2) + '\n')
        (output / f'{arm}-packing.json').write_text(json.dumps(packed, indent=2) + '\n')
        (output / f'{arm}-checks.json').write_text(json.dumps(checks, indent=2) + '\n')
        return checks
    finally:
        adapter.close()


def run(args):
    if not __debug__:
        raise RuntimeError('Smoke assertions require Python without -O')
    if platform.system() != 'Darwin' or not Path(SANDBOX).is_file():
        raise RuntimeError('This smoke requires macOS sandbox-exec; no unsandboxed fallback')
    binary, template, model = (path.resolve(strict=True) for path in
                               (args.binary, args.template, args.model_dir))
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    manifest = {'schema_version': 1, 'workload': 'synthetic-local-e1-v1',
                'workload_sha256': canonical_hash(workload()),
                'server_sha256': sha256(binary), 'template_sha256': sha256(template),
                'model_onnx_sha256': sha256(model / 'model.onnx'),
                'tokenizer_sha256': sha256(model / 'tokenizer.json'),
                'driver_sha256': sha256(Path(__file__)),
                'configuration_hashing': 'sha256 of canonical parsed TOML JSON',
                'adapter_sources_sha256': {name: sha256(Path(__file__).with_name(name + '.py'))
                    for name in ('runner', 'runner_local', 'runner_local_e1', 'runner_local_v2',
                                 'representations', 'evidence_v2', 'qa_context', 'qa_context_v2',
                                 'stage_diagnostics', 'longmemeval_dates', 'validate')},
                'network': 'macOS sandbox: outbound loopback only; Python DNS loopback only',
                'sandbox_profile_sha256': canonical_hash(PROFILE),
                'external_api_calls': 0, 'extraction_enabled': False,
                'platform': platform.system(), 'machine': platform.machine(),
                'python': platform.python_version(), 'quality_measured': False}
    with tempfile.TemporaryDirectory(prefix='loomem-e1-smoke-') as directory, loopback_only():
        temporary = Path(directory)
        wrapper = temporary / 'server-offline'
        wrapper.write_text('#!/bin/sh\nexec ' + shlex.join(
            [SANDBOX, '-p', PROFILE, str(binary)]) + '\n')
        wrapper.chmod(0o700)
        results, arm_settings = {}, {}
        for e1 in (False, True):
            arm = 'E1' if e1 else 'E0'
            arm_settings[arm] = settings_for(wrapper, template, model, temporary / arm)
            manifest[arm + '_preview_config_sha256'] = checked_config(
                arm_settings[arm], e1, temporary / arm)
        binding = canonical_hash(manifest)
        (args.output / 'inputs.json').write_text(json.dumps(manifest, indent=2) + '\n')
        for e1 in (False, True):
            arm = 'E1' if e1 else 'E0'
            results[arm] = run_arm(arm_settings[arm], e1, workload(), args.output, binding)
        manifest['inputs_sha256'] = binding
        manifest['status'] = 'PASS'
        for arm, result in results.items():
            manifest[arm + '_effective_config_sha256'] = result['effective_config_sha256']
        manifest['arms'] = {arm: {key: value for key, value in result.items()
                                 if key != 'stage_diagnostics'} for arm, result in results.items()}
    assert sha256(binary) == manifest['server_sha256'], 'Server artifact changed during smoke'
    manifest['temporary_databases_removed'] = not temporary.exists()
    manifest['child_processes_closed'] = True
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for name in ('binary', 'template', 'model-dir', 'output'):
        parser.add_argument('--' + name, required=True, type=Path)
    args = parser.parse_args()
    result = run(args)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
