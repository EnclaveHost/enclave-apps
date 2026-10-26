#!/usr/bin/env python3
"""Prepare a TEST-ONLY reduced-capacity allocator snapshot and service witness.

Never apply this to a production SDK or include it in prepare.py. The resulting
separate measured image must be labeled test-only and replaced after testing.
No fault shim, altered generator, plaintext pad, or bypass of verification is
introduced. The production allocator and error path are retained, with a small
fixed capacity and a diagnostic at the actual exhausted-counter branch.
"""
import argparse
import difflib
import hashlib
import json
from pathlib import Path
import shutil


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--service', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    destination = args.output / 'source'
    shutil.copytree(args.source, destination, symlinks=True)
    path = destination / 'shielded-tee.c'
    original = path.read_text()
    old = 'b->counter = 0; b->issued_hi = 0; b->capacity = UINT64_C(1) << 40;'
    new = 'b->counter = 0; b->issued_hi = 0; b->capacity = UINT64_C(16); /* TEST ONLY */'
    assert original.count(old) == 1
    modified = original.replace(old, new)
    old = 'if (b->counter >= b->capacity) { pthread_mutex_unlock(&b->mu); return SH_ERR_EXHAUST; }'
    new = '''if (b->counter >= b->capacity) {
        fprintf(stderr, "[test-only] actual maskbank exhausted: issued=%llu capacity=%llu\\n",
                (unsigned long long)b->counter, (unsigned long long)b->capacity);
        pthread_mutex_unlock(&b->mu); return SH_ERR_EXHAUST;
    }'''
    assert modified.count(old) == 1
    modified = modified.replace(old, new)
    path.write_text(modified)
    # Force this translation unit and the shared library to be rebuilt. Other
    # objects come from the previously audited production build.
    for name in ('shielded-tee.o', 'libggml-shielded.so'):
        (destination / name).unlink(missing_ok=True)
    service = args.service.read_text()
    anchor = '            stats = parse_native_stats(stderr)\n'
    assert service.count(anchor) == 1
    instrumented = service.replace(anchor, anchor + '''            # TEST ONLY: forward two fixed diagnostic witnesses, never job text.
            for witness in (
                '[test-only] actual maskbank exhausted: issued=16 capacity=16',
                '[shielded] protected offload failed (-11); backend retired',
            ):
                if witness in stderr:
                    print(witness, file=sys.stderr, flush=True)
''')
    (args.output / 'service.py').write_text(instrumented)
    for filename, before, after in [('allocator.patch', original, modified),
                                    ('service-witness.patch', service, instrumented)]:
        (args.output / filename).write_text(''.join(difflib.unified_diff(
            before.splitlines(True), after.splitlines(True),
            fromfile='production', tofile='test-only')))
    sha = lambda value: hashlib.sha256(value.encode()).hexdigest()
    receipt = {'schema': 'shield-exhaustion-fixture/1', 'testOnly': True,
        'productionCapacity': 1 << 40, 'testCapacity': 16,
        'productionSourceSha256': sha(original), 'fixtureSourceSha256': sha(modified),
        'productionServiceSha256': sha(service), 'fixtureServiceSha256': sha(instrumented),
        'generatorChanged': False, 'exhaustionReturnCodeInjected': False,
        'counterAdvancedArtificially': False, 'verificationDisabled': False}
    (args.output / 'fixture.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt))


if __name__ == '__main__':
    main()
