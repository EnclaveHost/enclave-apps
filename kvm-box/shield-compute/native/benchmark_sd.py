#!/usr/bin/env python3
"""Run a public SD fixture inside the desktop OCI image (host evidence only).

Mount this directory at /tests, the model directory read-only at /models and
an empty results directory at /results. Use the dedicated loopback 9741 worker
with Docker host networking. The parent service.py is imported from /shield.
No private prompt or model should be used by this benchmark.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=('cpu', 'narrow', 'wide'))
    parser.add_argument('--pad-profile', choices=('default', 'vector', 'batch32', 'batch32-vector'),
                        default='default', help='host-only pad-generation comparison; wide mode only')
    parser.add_argument('--output', type=Path, default=Path('/results'))
    args = parser.parse_args()
    if args.pad_profile != 'default' and args.mode != 'wide':
        parser.error('alternative pad profiles require wide mode')
    name = args.mode if args.pad_profile == 'default' else args.mode + '-' + args.pad_profile
    output = args.output / name
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
    service = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(service)
    config = json.loads(Path('/shield/service.json').read_text())
    config['worker'] = {'host': '127.0.0.1', 'port': 9741}
    config['threads'] = 2
    compute = service.Compute(config)
    env = compute.environment(config['models']['sd'], args.mode != 'cpu', str(output), 'sd')
    # Explicit A/B comparison only. Clients of the measured service cannot set
    # these variables; the production SD profile always enables chunking.
    if args.mode == 'narrow':
        env.pop('SHIELDED_WIDE', None)
    elif args.mode == 'wide':
        if env.get('SHIELDED_WIDE') != '1':
            raise RuntimeError('image does not enable wide SD in its service')
    if args.mode != 'cpu':
        env['SHIELDED_PROFILE'] = '1'
    # Existing implementation knobs only. The vector sampler runs its own
    # scalar-equivalence check before admission; neither profile changes keys,
    # issuance limits, verification, model arithmetic or public-data approval.
    if 'vector' in args.pad_profile:
        env['SHIELDED_MASK_CHACHA16'] = '1'
    if args.pad_profile.startswith('batch32'):
        env['SHIELDED_REFILL_BATCH'] = '32'
        env['SHIELDED_REFILL_UNIT'] = '16'
    prompt = 'A watercolor cabin beside a lake, pine trees, morning light'
    (output / 'prompt').write_text(prompt)
    command = ['/opt/shield/bin/sd-cli', '-m', config['models']['sd']['model'],
               '--prompt-file', str(output / 'prompt'), '-o', str(output / 'result.png'),
               '--width', '256', '--height', '256', '--steps', '8', '--cfg-scale', '7',
               '--sampling-method', 'euler', '--seed', '42', '--threads', '2',
               '--backend', 'cpu', '--mmap', '--diffusion-fa']
    start = time.monotonic()
    with (output / 'stdout').open('wb') as stdout, (output / 'stderr').open('wb') as stderr:
        process = subprocess.run(command, env=env, cwd=output, stdout=stdout, stderr=stderr, timeout=600)
    elapsed = time.monotonic() - start
    stats = service.parse_native_stats((output / 'stderr').read_text(errors='replace'))
    backend = Path('/opt/shield/lib/backends/libggml-shielded.so')
    record = {'schema': 'shield-sd-benchmark/1', 'mode': args.mode,
              'padProfile': args.pad_profile, 'seconds': elapsed,
              'exitCode': process.returncode, 'command': command, 'prompt': prompt,
              'environment': env, 'stats': stats,
              'backendSha256': hashlib.sha256(backend.read_bytes()).hexdigest(),
              'pngSha256': hashlib.sha256((output/'result.png').read_bytes()).hexdigest()
                  if (output/'result.png').is_file() else None}
    (output / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
    if process.returncode or (args.mode != 'cpu' and
            (not stats or not stats['offloaded'] or stats['verification_failures'])):
        raise RuntimeError('benchmark failed; inspect saved evidence')
    print(json.dumps(record), flush=True)


if __name__ == '__main__':
    main()
