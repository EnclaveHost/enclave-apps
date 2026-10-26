#!/usr/bin/env python3
"""Assert native SD failure/retirement through the deployed owner connector.

Arrange an unavailable dedicated worker, a disconnect proxy, or the separately
measured reduced-capacity allocator test image first. Verify attestation
separately. This script does not establish the fault's origin, change worker
services or restart guests. Restore the regular image/worker afterward.
"""
import argparse
import json
from pathlib import Path
import urllib.error
import urllib.request


def request(path, body=None):
    headers = {'X-KVM-Box-Request': '1'}
    if body is not None:
        headers['Content-Type'] = 'application/json'
    req = urllib.request.Request('http://127.0.0.1:34719/desktop/api/shield/' + path,
          data=None if body is None else json.dumps(body).encode(), headers=headers)
    try:
        response = urllib.request.urlopen(req, timeout=180)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return {'httpStatus': response.status, 'body': json.load(response)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fault', choices=('unavailable', 'disconnect', 'allocator-exhaustion'), required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    before = request('status')
    assert before['httpStatus'] == 200 and before['body']['state'] == 'ready', before
    job = {'op': 'sd', 'mode': 'masked', 'seed': 42,
           'prompt': 'A watercolor cabin beside a lake, pine trees, morning light'}
    if before['body'].get('nativeArithmetic'):
        job['arithmetic'] = before['body']['nativeArithmetic']['masked']
    failed = request('compute', job)
    retired = request('status')
    repeated = request('compute', job)
    cpu_job = {**job, 'mode': 'cpu'}
    if before['body'].get('nativeArithmetic'):
        cpu_job['arithmetic'] = before['body']['nativeArithmetic']['cpu']
    repeated_cpu = request('compute', cpu_job)
    after = request('status')
    record = {'schema': 'shield-vm-native-failure/1', 'fault': args.fault,
              'before': before, 'failed': failed, 'retired': retired,
              'repeated': repeated, 'repeatedCpu': repeated_cpu, 'after': after}
    args.output.write_text(json.dumps(record, indent=2) + '\n')
    for reply in (failed, repeated, repeated_cpu):
        # The local service reports application errors in a 200 JSON envelope.
        assert reply['httpStatus'] == 200 and reply['body'].get('ok') is False, reply
        assert not any(key in reply['body'] for key in ('png', 'text', 'outputs', 'stats')), reply
    assert 'compute failed' in failed['body'].get('error', ''), failed
    assert 'retired' in repeated['body'].get('error', ''), repeated
    assert 'retired' in repeated_cpu['body'].get('error', ''), repeated_cpu
    for status in (retired, after):
        assert status['body']['state'] == 'retired', status
        for key in ('completed', 'telemetry'):
            assert status['body'][key] == before['body'][key], status
    print(args.fault + ': failed job and repeated request rejected; no result or counters released')


if __name__ == '__main__':
    main()
