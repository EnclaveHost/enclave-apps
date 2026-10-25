#!/usr/bin/env python3
"""Build optional rejection hints using the installed source compiler twice.

This table can only select existing trusted CPU fallback, never an offload.
Run inside the target image with its application resource limits. Exact source
and options remain separate, length-delimited UTF-8 strings; no normalization.
The table is compiled into the frontend, with no runtime cache file or override.
"""
import argparse
import hashlib
import json
import os
import resource
from pathlib import Path
import subprocess
import sys

PREFIX = b'shield-source-decline-v1\0'


def fingerprint(source, options):
    digest = hashlib.sha256(PREFIX)
    for value in (source, options):
        encoded = value.encode('utf-8')
        digest.update(len(encoded).to_bytes(8, 'little'))
        digest.update(encoded)
    return digest.hexdigest()


def digest_file(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def compiler_result(compiler, request):
    raw = json.dumps(request, separators=(',', ':')).encode()
    result = subprocess.run([sys.executable, '-I', str(compiler)], input=raw,
                            capture_output=True, check=True, timeout=45)
    if len(result.stdout) > 65536:
        raise ValueError('compiler result exceeds frontend limit')
    value = json.loads(result.stdout)
    if not isinstance(value, dict):
        raise ValueError('invalid compiler result')
    return value


def check_canary_result(result, name, variant):
    if not isinstance(result, dict) or set(result) != {name} or not isinstance(result[name], dict):
        raise ValueError('positive canary result shape')
    plan = result[name]
    if variant is None:
        valid = (plan.get('schema') == 'shield-opencl-lowering/1' and
                 plan.get('operation') == 'gemm-i8-i8-i64' and
                 plan.get('dispatchable', True) is True and plan.get('inputBound') == 128 and
                 plan.get('dimensions') == 2 and
                 plan.get('arguments') == {'a': 0, 'b': 1, 'c': 2, 'm': 3, 'n': 4, 'k': 5})
    else:
        valid = (plan.get('schema') == 'shield-opencl-q8-analysis/1' and
                 plan.get('operation') == 'q8_0-f32-matmul' and plan.get('dispatchable') is False and
                 plan.get('variant') == variant and plan.get('arguments') ==
                 {'quants': 0, 'scales': 1, 'input': 2, 'inputOffset': 3, 'output': 4, 'outputOffset': 5})
    if not valid:
        raise ValueError('positive canary returned an invalid plan')


def check_canaries(compiler):
    root = compiler.parent
    cases = [{'source': (root / 'gemm_i8.cl').read_text(), 'options': options}
             for options in ('', '-cl-std=CL1.2')]
    if compiler.name == 'q8_capture_compiler.py':
        for name in ('mul_mm_q8_0_f32_l4_lm.cl', 'mul_mv_q8_0_f32_flat.cl'):
            for options in ('-cl-std=CL3.0', '-cl-std=CL3.0 -cl-mad-enable '
                            '-cl-unsafe-math-optimizations -cl-finite-math-only -cl-fast-relaxed-math'):
                cases.append({'source': (root / 'q8-reference' / name).read_text(), 'options': options})
    elif compiler.name != 'kernel_compiler.py':
        raise ValueError('unsupported compiler entry point')
    results = []
    for index, request in enumerate(cases):
        variant = None if index < 2 else ('mm' if index < 4 else 'mv')
        name = 'gemm' if variant is None else ('kernel_mul_mm_q8_0_f32_l4_lm' if variant == 'mm' else 'kernel_mul_mv_q8_0_f32_flat')
        value = compiler_result(compiler, request)
        check_canary_result(value, name, variant)
        results.append({'fingerprint': fingerprint(request['source'], request['options']),
                        'kernel': name, 'variant': variant,
                        'resultSha256': hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()})
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--requests', type=Path, required=True)
    parser.add_argument('--compiler', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        raise ValueError('output already exists')
    canaries = check_canaries(args.compiler)
    records = []
    rejected = set()
    outcomes = {}
    for path in sorted(args.requests.glob('*.json')):
        request_bytes = path.read_bytes()
        request = json.loads(request_bytes)
        if set(request) != {'source', 'options'} or any(type(v) is not str for v in request.values()):
            raise ValueError('invalid source request: ' + path.name)
        if len(request['source'].encode()) >= 1 << 20 or len(request['options'].encode()) >= 4096:
            raise ValueError('oversized source request')
        outputs = [compiler_result(args.compiler, request) for _ in range(2)]
        if outputs[0] != outputs[1] or not isinstance(outputs[0], dict):
            raise ValueError('unstable compiler result')
        key = fingerprint(request['source'], request['options'])
        if key in outcomes and outcomes[key] != outputs[0]:
            raise ValueError('conflicting compiler outcomes for one fingerprint')
        outcomes[key] = outputs[0]
        decline = outputs[0] == {}
        if decline:
            rejected.add(key)
        records.append({'requestFile': path.name,
                        'requestSha256': hashlib.sha256(request_bytes).hexdigest(),
                        'fingerprint': key, 'declinedBothRuns': decline,
                        'resultSha256': hashlib.sha256(json.dumps(outputs[0], sort_keys=True,
                                      separators=(',', ':')).encode()).hexdigest()})
    if not records:
        raise ValueError('no source requests')
    if check_canaries(args.compiler) != canaries:
        raise ValueError('compiler canary set changed')
    header = ('// Generated rejection hints; no authorization data.\n'
              + 'inline constexpr std::string_view compiler_path = '
              + json.dumps(str(args.compiler)) + ';\n'
              f'inline constexpr std::array<std::string_view, {len(rejected)}> fingerprints = {{\n'
              + ''.join('    "' + key + '",\n' for key in sorted(rejected)) + '};\n')
    dependencies = {str(p.relative_to(args.compiler.parent)): hashlib.sha256(p.read_bytes()).hexdigest()
                    for p in sorted(args.compiler.parent.rglob('*'))
                    if p.is_file() and p.suffix in ('.py', '.cl')}
    clang = Path('/usr/bin/clang-18')
    resource_dir = Path(subprocess.check_output([str(clang), '-print-resource-dir'], text=True).strip())
    environment = {
        'pythonVersion': sys.version, 'pythonSha256': digest_file(sys.executable),
        'clangVersion': subprocess.check_output([str(clang), '--version'], text=True).strip(),
        'clangSha256': digest_file(clang),
        'clangHeaders': {str(p.relative_to(resource_dir)): digest_file(p)
                         for p in sorted(resource_dir.rglob('*')) if p.is_file()},
        'limits': {name: resource.getrlimit(getattr(resource, name)) for name in
                   ('RLIMIT_CPU', 'RLIMIT_AS', 'RLIMIT_FSIZE', 'RLIMIT_CORE')},
        'environment': {name: os.environ.get(name) for name in ('LANG', 'LC_ALL', 'TMPDIR')},
    }
    manifest = {'schema': 'shield-source-declines/1', 'compiler': str(args.compiler),
                'compilerSources': dependencies, 'records': records,
                'environment': environment, 'positiveCanariesBeforeAndAfter': len(canaries), 'canaryResults': canaries,
                'headerSha256': hashlib.sha256(header.encode()).hexdigest(),
                'scope': 'rejection-only; original compiler handles all nonmatches; no offload grants'}
    args.output.mkdir(parents=True, exist_ok=False)
    (args.output / 'source_declines.inc').write_text(header)
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps({'requests': len(records), 'declines': len(rejected)}))


if __name__ == '__main__':
    main()
