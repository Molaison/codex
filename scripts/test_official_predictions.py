"""Regression checks for the new companion's authentication and no-fallback boundary."""
import base64
import hashlib
import io
import json
import os
import pathlib
import tempfile
import time
import types
import urllib.error
import unittest.mock as mock

module = types.ModuleType('official_prediction')
exec(compile((pathlib.Path(__file__).resolve().parents[1] / 'codex-rs/tui/src/app/official_prediction.py').read_text(encoding='utf-8'), 'official_prediction.py', 'exec'), module.__dict__)
passed = []
with tempfile.TemporaryDirectory() as temporary:
    root = pathlib.Path(temporary)
    (root / module.BACKEND_NAME).write_bytes(b'offline backend substitute')
    module.BACKEND_SHA256 = hashlib.sha256(b'offline backend substitute').hexdigest()
    request = {'model': 'gpt-6.1-sol', 'binary_directory': temporary, 'codex_home': temporary, 'runtime_root': temporary,
               'source_thread_id': '00000000-0000-4000-8000-000000000001', 'source_turn_id': '00000000-0000-4000-8000-000000000002',
               'prediction_trigger': 'live',
               'items': [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': 'Example?'}]}]}
    configuration = {'is_enabled': True, 'unsupported_models': [], 'live_sample_rate': 1,
                     'prompt': 'official remote prompt', 'prediction_reasoning_effort': 'high'}
    def run(case):
        claims = {'exp': time.time() + (10 if case == 'expired' else 3600), 'https://api.openai.com/auth': {'chatgpt_plan_type': 'pro'}}
        token = 'header.' + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=') + '.signature'
        auth = {'auth_mode': 'api' if case == 'api_login' else 'chatgpt',
                'tokens': {'access_token': token, 'id_token': 'id', 'account_id': 'test-account', 'refresh_token': 'must-never-propagate'}}
        original = json.dumps(auth).encode()
        (root / 'auth.json').write_bytes(original)
        config = dict(configuration, is_enabled=case != 'disabled')
        request['prediction_trigger'] = 'catch_up' if case.startswith('catch_up') else 'live'
        config['catch_up_sample_rate'] = 0 if case == 'catch_up_skipped' else None
        config['sample_rate'] = 1
        seed = {'modelProvider': 'openai', 'model': request['model'], 'thread': {'id': 'seed'}}
        fork = {'modelProvider': 'ywl' if case == 'wrong_fork_provider' else 'openai', 'model': request['model'], 'thread': {'id': 'prediction'}}
        rows = [{'id': 1, 'result': {}}, {'id': 2, 'result': {'account': {'type': 'chatgpt', 'planType': 'pro'}}},
                {'id': 3, 'result': seed}, {'id': 4, 'result': {}},
                {'id': 5, 'error': {'code': -32601}} if case == 'fork_unavailable' else {'id': 5, 'result': fork},
                {'id': 6, 'result': {}},
                {'method': 'item/completed', 'params': {'threadId': 'prediction', 'item': {'type': 'agentMessage', 'text': '{"suggestion":"Show the example"}'}}},
                {'method': 'turn/completed', 'params': {'threadId': 'prediction', 'turn': {'status': 'completed'}}}]
        process = mock.Mock(pid=123456, stdin=io.StringIO(), stdout=io.StringIO('\n'.join(json.dumps(row) for row in rows)))
        def popen(*args, **kwargs):
            env = kwargs['env']
            assert not any(key in env for key in ('OPENAI_API_KEY', 'CODEX_API_KEY', 'OPENAI_BASE_URL'))
            isolated = json.loads((pathlib.Path(env['CODEX_HOME']) / 'auth.json').read_text())
            assert isolated['tokens']['refresh_token'] == '' and not isolated.get('OPENAI_API_KEY')
            assert env['CODEX_HOME'] != temporary
            assert 'model_provider="openai"' in (pathlib.Path(env['CODEX_HOME']) / 'config.toml').read_text()
            return process
        opener = mock.Mock()
        opener.open.return_value = io.BytesIO(json.dumps(config).encode())
        if case == 'configuration_unreachable':
            opener.open.side_effect = urllib.error.URLError('synthetic network failure')
        output = io.StringIO()
        failure = None
        with mock.patch.object(module.sys, 'stdin', types.SimpleNamespace(buffer=io.BytesIO(json.dumps(request).encode() + b'\n'))), \
             mock.patch.object(module.sys, 'stdout', output), mock.patch.object(module.urllib.request, 'build_opener', return_value=opener), \
             mock.patch.object(module.subprocess, 'Popen', side_effect=popen) as spawn, mock.patch.object(module.os, 'killpg', create=True), \
             mock.patch.dict(os.environ, {'OPENAI_API_KEY': 'forbidden-api-key', 'CODEX_API_KEY': 'forbidden-key', 'OPENAI_BASE_URL': 'http://forbidden.invalid'}):
            try:
                module.main()
            except (RuntimeError, urllib.error.URLError) as error:
                failure = str(error)
        messages = [json.loads(line) for line in process.stdin.getvalue().splitlines()]
        turns = [row for row in messages if row.get('method') == 'turn/start']
        if case in ('success', 'catch_up_success'):
            assert failure is None, failure
            assert json.loads(output.getvalue()) == {'suggestion': 'Show the example'}
            assert len(turns) == 1 and turns[0]['params']['threadId'] == 'prediction'
            params = turns[0]['params']
            assert params['turnTrigger'] == 'composer_predictions' and params['effort'] == 'high'
            assert params['responsesapiClientMetadata']['prediction_source_thread_id'] == request['source_thread_id']
            assert params['responsesapiClientMetadata']['prediction_source_turn_id'] == request['source_turn_id']
            assert params['responsesapiClientMetadata']['prediction_trigger'] == request['prediction_trigger']
            assert params['clientUserMessageId'] and params['outputSchema'] is None
            assert params['input'][0]['text'].startswith(configuration['prompt'] + '\n\nReturn only JSON matching this schema: ')
            fork_request = next(row['params'] for row in messages if row.get('method') == 'thread/fork')
            assert fork_request == {'threadId': 'seed', 'experimentalPredictionMode': True, 'ephemeral': True, 'excludeTurns': True, 'threadSource': 'composer_predictions'}
        elif case == 'catch_up_skipped':
            assert failure is None and not turns and not spawn.called
            assert json.loads(output.getvalue()) == {'suggestion': None}
        else:
            assert failure and not turns, (case, failure, turns)
            if case in ('api_login', 'expired', 'disabled', 'configuration_unreachable'):
                assert not spawn.called
        assert (root / 'auth.json').read_bytes() == original
        assert not list(root.glob('companion-*'))
        passed.append(case)
    for case in ('success', 'catch_up_success', 'catch_up_skipped', 'api_login', 'expired', 'disabled', 'fork_unavailable', 'wrong_fork_provider', 'configuration_unreachable'):
        run(case)
report = {'passed': passed, 'network_requests': 0, 'inference_requests': 0, 'auth_unchanged': True, 'temporary_credentials_cleaned': True}
print(json.dumps(report))
