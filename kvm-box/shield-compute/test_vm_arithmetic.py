#!/usr/bin/env python3
"""Check native numeric consent through the deployed owner connector.

Requires separate attestation. No accepted computation is submitted here;
positive native and exact graph workloads are validated separately.
"""
import argparse
import json
from pathlib import Path

from test_vm_admission import Client, unchanged


def run(client):
    initial = client.status()
    assert initial['state'] == 'ready' and initial['active'] is None
    assert initial['nativeArithmetic'] == {
        'cpu': 'native-model-v1', 'masked': 'shield-fixed-point-v1',
        'maskedRequiresExplicitOptIn': True}
    cases = []
    for engine in ('llama', 'sd'):
        for name, value in [('missing', None), ('native', 'native-model-v1'),
                            ('unknown', 'shield-fixed-point-v2'),
                            ('boolean', True), ('object', {'approximate': True})]:
            request = {'op': engine, 'mode': 'masked', 'prompt': 'A public test fixture'}
            if value is not None:
                request['arithmetic'] = value
            code, result = client.request(request)
            assert code == 200 and result.get('ok') is False, (code, result)
            assert 'requires arithmetic=' in result.get('error', ''), result
            assert set(result) == {'ok', 'error'}, result
            unchanged(initial, client.status())
            cases.append({'engine': engine, 'case': name, 'response': result})
        code, result = client.request({'op': engine, 'mode': 'cpu',
            'prompt': 'A public test fixture', 'arithmetic': 'shield-fixed-point-v1'})
        assert code == 200 and result == {
            'ok': False, 'error': 'CPU mode requires native-model-v1 arithmetic'}, result
        unchanged(initial, client.status())
        cases.append({'engine': engine, 'case': 'cpu-contract-conflict', 'response': result})
    return {'schema': 'shield-vm-arithmetic-admission/1', 'cases': cases,
            'initialStatus': initial, 'finalStatus': client.status(),
            'successfulWorkCountersUnchanged': True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--origin', default='http://127.0.0.1:34719')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    receipt = run(Client(args.origin))
    args.output.write_text(json.dumps(receipt, indent=2) + '\n')
    print(f"Passed {len(receipt['cases'])} deployed native arithmetic admission cases")
