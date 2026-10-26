#!/usr/bin/env python3
"""Run controlled original Q8 kernels, then replay actual launches through admission.

Run in a networkless, CPU-only desktop-image container with the experimental
PoCL library and bitcode mounted. An exact ICD library hash is mandatory.
"""
import argparse
import hashlib
import json
from pathlib import Path
import resource
import subprocess

from q8_admission import BufferView, validate_launch


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--reference', type=Path, required=True)
    parser.add_argument('--expected-pocl-sha', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    icd = Path('/opt/shield/pocl/lib/libpocl.so.2')
    digest = sha(icd)
    if digest != args.expected_pocl_sha:
        parser.error('actual PoCL ICD library digest mismatch')
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_FSIZE, (8 << 20, 8 << 20))
    env = {'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'LANG': 'C.UTF-8',
           'POCL_MAX_PTHREAD_COUNT': '2', 'POCL_CPU_MAX_CU_COUNT': '2',
           'POCL_CACHE_DIR': str(args.output.parent / 'pocl-cache')}
    stdout_path = args.output.with_suffix('.stdout')
    stderr_path = args.output.with_suffix('.stderr')
    with stdout_path.open('wb') as stdout, stderr_path.open('wb') as stderr:
        result = subprocess.run([str(args.binary), str(args.reference)],
                                env=env, stdout=stdout, stderr=stderr, timeout=120)
    if result.returncode:
        raise SystemExit(f'Q8 execution failed: {result.returncode}; see {stderr_path}')
    records = [json.loads(line) for line in stdout_path.read_text().splitlines() if line.startswith('{')]
    if len(records) != 12 or any(not record['exact'] or not record['guards'] for record in records):
        raise AssertionError('missing or failed execution cases')
    for record in records:
        scalars = {int(index): value for index, value in record['scalars'].items()}
        buffers = {index: BufferView(index+1, size, 0, size)
                   for index, size in zip((0, 1, 2, 4), record['bufferBytes'])}
        launch = validate_launch(record['variant'], scalars, buffers, record['global'], record['local'])
        if record['outputs'] != launch.m * launch.n:
            raise AssertionError('output coverage mismatch')
    summary = {'schema': 'shield-q8-kernel-probe/1', 'exitCode': result.returncode,
               'scope': 'CPU OpenCL; exact dyadic test values, not arbitrary-float accuracy',
               'maskedDispatch': False, 'cases': records,
               'checkedOutputs': sum(record['outputs'] for record in records),
               'validatedLaunches': len(records), 'poclLibrarySha256': digest,
        'frontendLibrarySha256': hashlib.sha256(Path('/opt/shield/opencl/lib/libOpenCL.so.1').read_bytes()).hexdigest(),
               'kernelBitcodeSha256': sha('/opt/shield/pocl/share/pocl/kernel-x86_64-pc-linux-gnu-x86-64.bc'),
               'binarySha256': sha(args.binary),
               'sourceSha256': {path.name: sha(path) for path in sorted(args.reference.glob('*.cl'))}}
    args.output.write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps({key: value for key, value in summary.items() if key != 'cases'}))


if __name__ == '__main__':
    main()
