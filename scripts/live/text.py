#!/usr/bin/env python3
"""Painted text assertions and observation retry against a disposable HTTP target."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import tempfile
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse
from urllib.request import urlopen

REPO = Path(__file__).resolve().parents[2]
os.umask(0o077)
BINARY = REPO / 'target/debug/manuvra'
ROOT = REPO / '.work/live/text' / f'{time.strftime("%Y%m%d-%H%M%S")}-{os.getpid()}'
FIXTURE = (REPO / 'tests/browser/text.html').read_text()
REVISION = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=REPO, text=True).strip()
PAINT = threading.Event()
ACK = threading.Event()
ACTIVE = {}
SUMMARY = []


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        request = urlparse(self.path)
        case = parse_qs(request.query).get('case', [''])[0]
        if request.path == '/paint':
            PAINT.set()
            body = b'paint enabled'
        elif request.path == '/state':
            body = json.dumps({'paint': PAINT.is_set()}).encode()
        elif request.path == '/ack':
            ACK.set()
            body = b'paint observed'
        else:
            body = FIXTURE
            if case == 'retry':
                body += '''<script>
const amount=document.querySelector('.amount');
amount.style.clipPath='circle(10px)';
const timer=setInterval(async()=>{
 if((await(await fetch('/state')).json()).paint){
  clearInterval(timer);amount.style.clipPath='none';await fetch('/ack');
 }
},100);
</script>'''
            body = body.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def command(case, label, args):
    env = os.environ.copy()
    env.pop('TYPESAFE_API_KEY', None)
    env['XDG_STATE_HOME'] = str(ROOT / case / 'state')
    env['XDG_RUNTIME_DIR'] = str(RUNTIME / case)
    result = subprocess.run([str(BINARY), *args], env=env, capture_output=True, timeout=100)
    (ROOT / case / f'{label}.stdout.json').write_bytes(result.stdout)
    (ROOT / case / f'{label}.stderr.txt').write_bytes(result.stderr)
    value = json.loads(result.stdout)
    assert result.returncode in (0, 2, 3, 4, 5, 6), value
    if value.get('run_id'):
        ACTIVE[case] = value
    return value


def settle(case, value):
    for index in range(8):
        if value['state'] != 'running':
            return value
        value = command(case, f'status-{index}', ['status', value['run_id'], '--wait-ms', '30000'])
    raise AssertionError(f'{case}: run did not settle')


def run(case, assertion, secret=False, natural=False):
    directory = ROOT / case
    directory.mkdir(parents=True)
    wire = {
        'schema_version': 1, 'target': {'kind': 'browser', 'url': f'{ORIGIN}/ready?case={case}'},
        'context': {'journey': 'Synthetic painted text', 'revision': REVISION,
                    'environment': 'disposable loopback HTTP', 'actor': 'owner', 'authority': 'observe'},
        'steps': [{'id': 'ready', 'goal': 'Observe Ready', 'done_when': [{'text_visible': 'Ready'}]}],
        'expectations': [{'id': 'text', 'assertions': [assertion]}],
        'options': {'allowed_origins': [ORIGIN], 'active_timeout_ms': 30000,
                    'pause_timeout_ms': 30000, 'lifetime_ms': 90000},
    }
    if secret:
        wire['values'] = {'marker': {'value': '$0.00', 'description': 'classified amount', 'secret': True}}
        wire['steps'][0]['done_when'] = [assertion]
    if natural:
        wire['expectations'].append({'id': 'natural', 'claim': 'Ready is present'})
    write(directory / 'job.json', wire)
    return settle(case, command(case, 'run', ['run', '--request-id', f'text-{case}-{ROOT.name}',
        '--job', str(directory / 'job.json'), '--evidence', str(directory / 'evidence'),
        '--headless', '--wait-ms', '1000']))


def artifacts(value):
    manifest = json.loads(Path(value['evidence']['manifest']).read_text())
    assert manifest['complete'] and value['evidence']['complete']
    result = {}
    for artifact in manifest['artifacts']:
        body = Path(artifact['path']).read_bytes()
        assert artifact['complete'] and hashlib.sha256(body).hexdigest() == artifact['digest']
        result.setdefault(artifact['role'], []).append(body)
    return result


def terminal(case, value, state, channel=None, secret=False):
    assert value['state'] == state and value['terminal'], value
    assert not value['verdict']['caller_assisted'], value
    assert value['cleanup']['browser'] == 'closed' and value['cleanup']['profile'] == 'removed', value
    records = artifacts(value)
    verification = json.loads(records['verification'][0])
    assert verification['provider'] is None and not records.get('decision'), 'structured checks called a provider'
    traces = [json.loads(line) for line in records['trace'][0].splitlines()]
    assert not any(entry.get('event') == 'action_prepared' for entry in traces), 'read-only job mutated'
    checks = verification['expectations'][0]['assertion_checks']
    assert checks == value['verdict']['expectations'][0]['assertion_checks']
    assert checks[0]['text']['matched_channel'] == channel, checks
    if secret:
        assert all(b'$0.00' not in body for bodies in records.values() for body in bodies), 'classified amount leaked'
    SUMMARY.append({'case': case, 'state': state, 'channel': channel, 'provider_calls': 0,
        'mutations': 0, 'caller_assisted': False, 'manifest': value['evidence']['manifest'],
        'digests_verified': True, 'cleanup': value['cleanup']})
    return verification


def abort(case, value):
    return settle(case, command(case, 'abort', ['abort', value['run_id'], '--request-id', f'abort-{case}-{ROOT.name}']))


def matrix():
    for case, assertion, state, channel in [
        ('default', {'text_visible': '$0.00'}, 'failed', None),
        ('painted', {'text_visible': '$0.00', 'include_aria_hidden': True}, 'passed', 'painted_aria_hidden'),
        ('absent-default', {'text_absent': '$0.00'}, 'passed', None),
        ('absent-painted', {'text_absent': '$0.00', 'include_aria_hidden': True}, 'failed', 'painted_aria_hidden'),
        ('both', {'text_visible': '$2.00', 'include_aria_hidden': True}, 'passed', 'accessible'),
        ('inert', {'text_absent': '$3.00', 'include_aria_hidden': True}, 'passed', None),
        ('legacy-dialog', {'text_visible': '$21.00', 'scope': {'dialog': 'Month details'}}, 'passed', 'dialog_text'),
        ('strict-dialog', {'text_visible': '$21.00', 'scope': {'dialog': 'Month details'}, 'include_aria_hidden': True}, 'failed', None),
        ('painted-dialog', {'text_visible': '$20.00', 'scope': {'dialog': 'Month details'}, 'include_aria_hidden': True}, 'passed', 'painted_aria_hidden'),
        ('secret', {'text_visible': '$0.00', 'include_aria_hidden': True}, 'passed', 'painted_aria_hidden'),
        ('mixed-false', {'text_visible': 'Missing', 'include_aria_hidden': True}, 'failed', None),
    ]:
        report = terminal(case, run(case, assertion, secret=case == 'secret', natural=case == 'mixed-false'),
            state, channel, secret=case == 'secret')
        if case == 'mixed-false':
            assert report['expectations'][1]['result'] == 'not_run'
    value = run('retry', {'text_visible': '$0.00', 'include_aria_hidden': True})
    assert value['state'] == 'uncertain' and value['escalation']['dispositions'] == ['retry_observation', 'abort'], value
    urlopen(f'{ORIGIN}/paint', timeout=5).read()
    assert ACK.wait(5), 'fixture did not confirm its paint change'
    path = ROOT / 'retry/disposition.json'
    write(path, {'schema_version': 1, 'escalation_id': value['escalation']['id'], 'disposition': {'kind': 'retry_observation'}})
    value = settle('retry', command('retry', 'resume', ['resume', value['run_id'], '--request-id',
        f'resume-retry-{ROOT.name}', '--input', str(path)]))
    terminal('retry', value, 'passed', 'painted_aria_hidden')


if __name__ == '__main__':
    ROOT.mkdir(parents=True)
    runtime = tempfile.TemporaryDirectory(prefix='mvt-', dir='/tmp')
    RUNTIME = Path(runtime.name)
    subprocess.run(['cargo', 'build', '--locked', '--bin', 'manuvra'], cwd=REPO, check=True)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    ORIGIN = f'http://127.0.0.1:{server.server_port}'
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        matrix()
    finally:
        write(ROOT / 'summary.json', {'revision': REVISION,
            'binary_sha256': hashlib.sha256(BINARY.read_bytes()).hexdigest(),
            'fixture_sha256': hashlib.sha256(FIXTURE.encode()).hexdigest(), 'cases': SUMMARY})
        for case, value in list(ACTIVE.items()):
            if value.get('run_id') and not value.get('terminal'):
                try:
                    abort(case, value)
                except (AssertionError, subprocess.TimeoutExpired):
                    pass
        server.shutdown()
        server.server_close()
        runtime.cleanup()
    print(f'text live evidence: {ROOT / "summary.json"}')
