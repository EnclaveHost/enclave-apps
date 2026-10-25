#!/usr/bin/env python3
"""Bounded admission checks against an already-attested local development VM.

Uses the owner connector, one public SD fixture, at most eight simultaneous
requests, and a final CPU graph. Does not restart services or inject GPU faults.
Run only against the dedicated KVM Box development instance while otherwise idle.
Attestation is a separate prerequisite, not something this HTTP test proves.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import time
import urllib.error
import urllib.parse
import urllib.request


class Client:
    def __init__(self, origin):
        url = urllib.parse.urlsplit(origin)
        if (url.scheme != 'http' or url.hostname != '127.0.0.1' or not url.port
                or url.username or url.password or url.path or url.query or url.fragment):
            raise ValueError('use the loopback development owner connector origin')
        self.origin = origin

    def request(self, value=None, raw=None):
        path = '/desktop/api/shield/status' if value is None and raw is None else '/desktop/api/shield/compute'
        data = raw if raw is not None else json.dumps(value).encode() if value is not None else None
        request = urllib.request.Request(self.origin + path, data=data,
            headers={'X-KVM-Box-Request': '1', 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, {'httpError': error.read(1024).decode(errors='replace')}

    def status(self):
        code, result = self.request()
        assert code == 200 and result.get('ok'), (code, result)
        return result


def unchanged(before, after):
    assert after['state'] == 'ready' and after['active'] is None, after
    assert after['completed'] == before['completed'], (before, after)
    assert after['telemetry'] == before['telemetry'], (before, after)


def run(client):
    initial = client.status()
    assert initial['state'] == 'ready' and initial['active'] is None, initial
    native_contract = {'arithmetic': initial['nativeArithmetic']['masked']} if initial.get('nativeArithmetic') else {}
    invalid = [
        ('non-object', []), ('unknown-engine', {'op': 'cuda'}),
        ('missing-prompt', {'op': 'llama'}),
        ('empty-prompt', {'op': 'llama', 'prompt': ''}),
        ('non-string-prompt', {'op': 'llama', 'prompt': ['test']}),
        ('nul-prompt', {'op': 'llama', 'prompt': 'test\0'}),
        ('oversize-prompt', {'op': 'llama', 'prompt': 'x' * 8193}),
        ('alternate-worker', {'op': 'llama', 'prompt': 'test', 'worker': '127.0.0.1:9741'}),
        ('environment-injection', {'op': 'llama', 'prompt': 'test', 'env': {'SHIELDED_VERIFY': '0'}}),
        ('raw-backend', {'op': 'llama', 'prompt': 'test', 'mode': 'cuda'}),
        ('negative-seed', {'op': 'llama', 'prompt': 'test', 'seed': -1}),
        ('boolean-seed', {'op': 'llama', 'prompt': 'test', 'seed': True}),
        ('oversize-seed', {'op': 'llama', 'prompt': 'test', 'seed': 1 << 31}),
        ('negative-tokens', {'op': 'llama', 'prompt': 'test', 'tokens': -1}),
        ('boolean-tokens', {'op': 'llama', 'prompt': 'test', 'tokens': True}),
        ('invalid-graph', {'op': 'graph', 'graph': {}, 'inputs': {}, 'mode': 'masked'}),
        ('graph-policy-injection', {'op': 'graph', 'graph': {}, 'inputs': {}, 'mode': 'masked', 'publicConstants': []}),
    ]
    rejected = []
    for name, request in invalid:
        code, result = client.request(request)
        assert code == 200 and result.get('ok') is False and 'error' in result, (name, code, result)
        assert not any(k in result for k in ('text', 'png', 'outputs', 'stats')), (name, result)
        unchanged(initial, client.status())
        rejected.append({'case': name, 'response': result})
    code, result = client.request(raw=b'{invalid-json')
    assert code == 400, (code, result)
    unchanged(initial, client.status())
    rejected.append({'case': 'invalid-json', 'httpStatus': code})

    # Observe authoritative busy state before issuing contenders, rather than
    # relying on a sleep to guess that the native engine has started.
    with ThreadPoolExecutor(max_workers=9) as pool:
        active = pool.submit(client.request, {'op': 'sd', 'mode': 'masked',
            'prompt': 'A watercolor cabin beside a lake, pine trees, morning light', 'seed': 42, **native_contract})
        deadline = time.monotonic() + 15
        while True:
            busy = client.status()
            if busy['state'] == 'busy' and busy.get('active', {}).get('engine') == 'sd':
                break
            assert not active.done() and time.monotonic() < deadline, 'did not observe active SD job'
            time.sleep(0.05)
        contenders = [pool.submit(client.request, {'op': 'llama', 'prompt': 'The capital of France is', 'tokens': 16})
                      for _ in range(8)]
        responses = [future.result() for future in contenders]
        assert all(code == 200 and r == {'ok': False, 'error': 'compute busy'} for code, r in responses), responses
        during = client.status()
        assert during['state'] == 'busy' and during['completed'] == initial['completed'], during
        assert during['telemetry'] == initial['telemetry'], during
        code, result = active.result()
    assert code == 200 and result.get('ok') and result.get('png'), result
    stats = result['stats']
    assert stats['exchanges'] > 0 and stats['macs'] > 0 and stats['verification_failures'] == 0, stats
    after = client.status()
    assert after['state'] == 'ready' and after['active'] is None, after
    assert after['completed'] == initial['completed'] + 1, after
    for key, field in (('verifiedOffloads', 'exchanges'), ('maskedMacs', 'macs')):
        assert after['telemetry'][key] - initial['telemetry'][key] == stats[field], after

    # A later job succeeds without restarting the service. No GPU access is
    # needed for this exact CPU-only add, so verified totals must remain fixed.
    graph = {'schema': 'shield-graph/1', 'nodes': [
        {'name': 'x', 'op': 'input', 'shape': [2], 'bound': 10},
        {'name': 'y', 'op': 'add', 'args': ['x', 'x']}], 'outputs': ['y']}
    code, local = client.request({'op': 'graph', 'mode': 'masked', 'graph': graph, 'inputs': {'x': [3, -4]}})
    assert code == 200 and local.get('ok') and local['outputs'] == {'y': [6, -8]}, local
    assert local['routes'] == {'y': 'local'} and local['stats'] == {}, local
    final = client.status()
    assert final['state'] == 'ready' and final['completed'] == initial['completed'] + 2, final
    for key in ('verifiedOffloads', 'maskedMacs'):
        assert final['telemetry'][key] == after['telemetry'][key], final
    import base64
    return {'schema': 'shield-vm-admission/1', 'initialStatus': initial,
            'invalidRequests': rejected, 'concurrentRejections': len(responses),
            'nativeStats': stats, 'pngSha256': hashlib.sha256(base64.b64decode(result['png'])).hexdigest(),
            'localRecovery': local, 'finalStatus': final,
            'scope': 'One busy SD job, eight contenders, invalid request rejection and later local job; not all concurrency schedules or pad exhaustion.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--origin', default='http://127.0.0.1:34719')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    receipt = run(Client(args.origin))
    receipt['testSourceSha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, indent=2) + '\n')
    print(f"Passed: {len(receipt['invalidRequests'])} invalid requests, {receipt['concurrentRejections']} busy contenders, exact counters, later local job.")


if __name__ == '__main__':
    main()
