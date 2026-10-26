#!/usr/bin/env python3
"""Approve exact public GGUF tensors, binding model and calibration digests.

This offline tool requires an explicit public-disclosure decision. Its output
must be authenticated by the measured guest configuration before use.
"""
import argparse
import hashlib
import json
from pathlib import Path

from gguf import GGUFReader

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--model', type=Path, required=True)
p.add_argument('--calibration', type=Path, required=True)
p.add_argument('--expected-model-sha256', required=True)
p.add_argument('--prefix', default='')
p.add_argument('--approve-public', action='store_true', required=True)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()

with a.model.open('rb') as f:
    digest = hashlib.file_digest(f, 'sha256').hexdigest()
if digest != a.expected_model_sha256:
    raise SystemExit('model digest differs from the approved artifact')
reader = GGUFReader(str(a.model))
entries = []
names = set()
for t in reader.tensors:
    name = a.prefix + t.name
    if (not name.isascii() or any(c.isspace() for c in name) or
            len(name) >= 128 or name in names):
        raise SystemExit('invalid, duplicate or truncated tensor name')
    names.add(name)
    dimensions = [int(n) for n in t.shape]
    dimensions += [1] * (4 - len(dimensions))
    content = memoryview(t.data).cast('B')
    h = hashlib.sha256()
    for offset in range(0, len(content), 1 << 20):
        h.update(content[offset:offset + (1 << 20)])
    entries.append({'name': name, 'type': int(t.tensor_type), 'ne': dimensions,
                    'nbytes': int(t.n_bytes), 'sha256': h.hexdigest()})
policy = {'schema': 'shield-public-tensors/1', 'modelSha256': digest,
          'calibrationSha256': hashlib.sha256(a.calibration.read_bytes()).hexdigest(),
          'tensors': entries}
a.output.write_text(json.dumps(policy, sort_keys=True, separators=(',', ':')) + '\n')
print(json.dumps({'tensors': len(entries),
                  'policySha256': hashlib.sha256(a.output.read_bytes()).hexdigest()}))
