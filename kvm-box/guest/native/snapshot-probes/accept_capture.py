"""Accept only a complete capture/read/discard and verified owned-VM cleanup."""
import json
import re
from pathlib import Path


def accept(report, before, after):
    r = report.get('result.json', {})
    watchdog = report.get('watchdog-receipt.json', {})
    if report.get('watchdog-error.json') or r.get('error') or r.get('cleanupError'):
        raise ValueError('guest lifecycle failure')
    if not r.get('cleanup', {}).get('absent') or not watchdog.get('absent') or not watchdog.get('secondAbsenceCheck', {}).get('absent'):
        raise ValueError('cleanup not confirmed twice')
    identity = r.get('id', '')
    if not re.fullmatch(r'[0-9a-fA-F-]{36}', identity):
        raise ValueError('missing owned VM identity')
    initial = {v['Id'].lower(): v['State'] for v in before['vms']}
    final = {v['Id'].lower(): v['State'] for v in after['vms']}
    if identity.lower() in initial or identity.lower() in final or initial != final:
        raise ValueError('preexisting inventory changed or diagnostic VM remains')
    if r.get('cleanup', {}).get('id') != identity:
        raise ValueError('cleanup identity mismatch')
    for key in ['inputs']:
        pins_before = {v['path']: v['sha256'] for v in before[key]}
        pins_after = {v['path']: v['sha256'] for v in after[key]}
        if not pins_before or pins_before != pins_after:
            raise ValueError('host input pins changed')
    master = next((h for p, h in pins_before.items() if p.endswith('\\type1.vmgs')), None)
    if not master or r.get('masterSha') != master:
        raise ValueError('master VMGS changed')
    lines = r.get('capture', '').splitlines()
    # Sysfs failures are observations, never security claims. Operation errors fail.
    if any(re.search(r'^SNAPPROBE (?!/)(?:.*ERROR|unexpected|image exceeds)', s) for s in lines):
        raise ValueError('snapshot operation error')
    expressions = [
        r'SNAPPROBE BEGIN capture-discard-v1',
        r'SNAPPROBE FREEZE begin',
        r'SNAPPROBE CREATE_IMAGE begin',
        r'SNAPPROBE CREATE_IMAGE OK original=1',
        r'SNAPPROBE IMAGE_SIZE ([0-9]+)',
        r'SNAPPROBE IMAGE_READ OK bytes=([0-9]+) diagnostic_fnv64=[0-9a-f]{16}',
        r'SNAPPROBE FREE OK', r'SNAPPROBE UNFREEZE OK', r'SNAPPROBE CLOSE OK',
        r'SNAPPROBE RESULT PASS', r'SNAPPROBE COMPLETE capture-discard-v1',
    ]
    position = -1
    matches = []
    for expression in expressions:
        found = [(i, re.fullmatch(expression, s)) for i, s in enumerate(lines)]
        found = [(i, m) for i, m in found if m]
        if len(found) != 1 or found[0][0] <= position:
            raise ValueError('missing, duplicate or out-of-order operation result')
        position, match = found[0]
        matches.append(match)
    size, read = int(matches[4][1]), int(matches[5][1])
    if size != read or not 0 < size <= 512 * 1024**2:
        raise ValueError('image read size invalid')
    return {'captureReadDiscardQualified': True, 'restoreQualified': False,
            'privateSnapshotQualified': False, 'bytes': size, 'imagePersisted': False}


if __name__ == '__main__':
    import argparse
    root = Path(__file__).resolve().parent / 'evidence'
    parser = argparse.ArgumentParser()
    parser.add_argument('--report', type=Path, default=root / 'capture-result.json')
    parser.add_argument('--before', type=Path, default=root / 'capture-before.json')
    parser.add_argument('--after', type=Path, default=root / 'capture-after.json')
    args = parser.parse_args()
    print(json.dumps(accept(*[json.loads(p.read_text()) for p in
        [args.report, args.before, args.after]]), indent=2))
