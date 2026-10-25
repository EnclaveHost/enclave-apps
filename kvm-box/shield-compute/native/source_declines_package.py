#!/usr/bin/env python3
"""Release gates for immutable, rejection-only compiler hints.

Build inputs are trusted release inputs. These checks prevent accidental stale
or mismatched tables; they do not turn an untrusted builder into a trust anchor.
No successful check grants GPU work. Actual offloads keep their runtime checks.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import subprocess
import sys
from functools import partial

COMPILER = '/opt/shield/opencl/q8_capture_compiler.py'
FRONTEND_SOURCES = ('shield_opencl.cpp', 'source_declines.hpp', 'build_source_declines.py',
                    'q8_capture.inc', 'q8_masked.inc', 'opencl.map', 'Makefile')
COMPILER_SOURCES = ('gemm_i8.cl', 'kernel_compiler.py', 'q8_admission.py',
                    'q8_capture_compiler.py', 'q8_kernel_compiler.py',
                    'q8-reference/mul_mm_q8_0_f32_l4_lm.cl',
                    'q8-reference/mul_mv_q8_0_f32_flat.cl')
FILE_LIMIT = 8 << 20


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def fingerprint(source, options):
    digest = hashlib.sha256(b'shield-source-decline-v1\0')
    for value in (source, options):
        encoded = value.encode('utf-8')
        digest.update(len(encoded).to_bytes(8, 'little'))
        digest.update(encoded)
    return digest.hexdigest()


def read_table(directory, requests):
    directory, requests = Path(directory), Path(requests)
    manifest = json.loads((directory / 'manifest.json').read_text())
    require(manifest.get('schema') == 'shield-source-declines/1', 'table schema')
    require(manifest.get('compiler') == COMPILER, 'compiler entry point')
    require(manifest.get('positiveCanariesBeforeAndAfter') == 6, 'missing positive canaries')
    controls = manifest.get('canaryResults', [])
    require(len(controls) == 6, 'missing canary result bindings')
    for index, control in enumerate(controls):
        variant = None if index < 2 else ('mm' if index < 4 else 'mv')
        name = 'gemm' if variant is None else ('kernel_mul_mm_q8_0_f32_l4_lm' if variant == 'mm' else 'kernel_mul_mv_q8_0_f32_flat')
        require(control['kernel'] == name and control['variant'] == variant and
                re.fullmatch(r'[0-9a-f]{64}', control['resultSha256']) is not None and
                re.fullmatch(r'[0-9a-f]{64}', control['fingerprint']) is not None, 'canary result shape')
    require(set(manifest['compilerSources']) == set(COMPILER_SOURCES), 'compiler source set')
    require(manifest['environment']['limits']['RLIMIT_FSIZE'] == [FILE_LIMIT, FILE_LIMIT],
            'table generated with different file limits')
    require(manifest['environment']['limits']['RLIMIT_CORE'] == [0, 0], 'core limit')
    require(bool(manifest['records']), 'empty source corpus')
    declines, seen, outcomes = set(), set(), {}
    for record in manifest['records']:
        name = record['requestFile']
        require(re.fullmatch(r'[0-9a-f]{64}\.json', name) is not None and name not in seen,
                'invalid/duplicate source filename')
        seen.add(name)
        raw = (requests / name).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == record['requestSha256'], 'request digest')
        request = json.loads(raw)
        require(set(request) == {'source', 'options'} and all(type(v) is str for v in request.values()),
                'source request shape')
        require(len(request['source'].encode()) < 1 << 20 and
                len(request['options'].encode()) < 4096, 'source request limits')
        key = fingerprint(request['source'], request['options'])
        require(key == record['fingerprint'], 'request fingerprint')
        require(type(record['declinedBothRuns']) is bool, 'decline flag')
        require(re.fullmatch(r'[0-9a-f]{64}', record['resultSha256']) is not None, 'result digest')
        outcome = (record['declinedBothRuns'], record['resultSha256'])
        require(key not in outcomes or outcomes[key] == outcome, 'conflicting source outcomes')
        outcomes[key] = outcome
        if record['declinedBothRuns']:
            require(record['resultSha256'] == hashlib.sha256(b'{}').hexdigest(), 'nonempty decline')
            declines.add(key)
    require({p.name for p in requests.glob('*.json')} == seen, 'source corpus membership')
    header = ('// Generated rejection hints; no authorization data.\n'
              + 'inline constexpr std::string_view compiler_path = '
              + json.dumps(COMPILER) + ';\n'
              f'inline constexpr std::array<std::string_view, {len(declines)}> fingerprints = {{\n'
              + ''.join('    "' + key + '",\n' for key in sorted(declines)) + '};\n')
    require((directory / 'source_declines.inc').read_bytes() == header.encode(),
            'header is not the canonical rejection table')
    require(sha(directory / 'source_declines.inc') == manifest['headerSha256'], 'header digest')
    return manifest


def marker(frontend):
    result = subprocess.run(['readelf', '--string-dump=.shield_declines', str(frontend)],
                            capture_output=True, text=True, check=True)
    values = re.findall(r'shield-source-declines/1:([0-9a-f]{64})(?=\s|$)', result.stdout)
    require(len(values) == 1, 'missing/ambiguous frontend table marker')
    return values[0]


def verify_build(build, frontend, source_root=None):
    build = Path(build)
    manifest = read_table(build / 'table', build / 'requests')
    receipt = json.loads((build / 'build-receipt.json').read_text())
    require(receipt['schema'] == 'shield-source-declines-build/1', 'frontend receipt schema')
    require(re.fullmatch(r'sha256:[0-9a-f]{64}', receipt['builderImage']) is not None, 'builder image identity')
    require(sha(build / 'command.json') == receipt['buildCommandSha256'], 'build command digest')
    require(sha(frontend) == receipt['frontendSha256'], 'frontend digest')
    require(sha(Path(frontend).parent / 'libOpenCL-shield-cpu.so.1') == receipt['cpuLoaderSha256'],
            'CPU loader digest')
    require(sha(build / 'table/manifest.json') == receipt['tableManifestSha256'], 'manifest digest')
    require(sha(build / 'table/source_declines.inc') == receipt['headerSha256'], 'build header digest')
    require(marker(frontend) == receipt['tableManifestSha256'], 'embedded table binding')
    require(set(receipt['sources']) == set(FRONTEND_SOURCES), 'frontend source set')
    if source_root is not None:
        require(sha(Path(source_root).parent / 'native/build-declines-frontend.py') == receipt['builderRecipeSha256'],
                'stale frontend build recipe')
        for name, digest in receipt['sources'].items():
            require(sha(Path(source_root) / name) == digest, 'stale frontend source: ' + name)
        for name, digest in manifest['compilerSources'].items():
            require(sha(Path(source_root) / name) == digest, 'stale compiler source: ' + name)
    return manifest, receipt


def desktop_identity(file_limit):
    resource.setrlimit(resource.RLIMIT_FSIZE, (file_limit, file_limit))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    os.setgroups([])
    os.setgid(1000)
    os.setuid(1000)


def installed_result(request, file_limit):
    result = subprocess.run(['/usr/local/bin/shield-sandbox', '/usr/bin/python3', '-I', COMPILER],
                            input=json.dumps(request, separators=(',', ':')).encode(),
                            capture_output=True, check=True, timeout=45,
                            preexec_fn=partial(desktop_identity, file_limit),
                            env={'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'LANG': 'C.UTF-8'})
    require(len(result.stdout) <= 65536, 'compiler output exceeds frontend limit')
    value = json.loads(result.stdout)
    require(isinstance(value, dict), 'invalid installed compiler result')
    return value


def sandbox_probe(file_limit):
    parent = dict(line.split(':', 1) for line in Path('/proc/self/status').read_text().splitlines())
    code = """import ctypes,json,os,resource,socket
from pathlib import Path
status=dict(line.split(':',1) for line in Path('/proc/self/status').read_text().splitlines())
try: socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM)
except PermissionError: denied=True
else: denied=False
print(json.dumps({'uid':os.getuid(),'euid':os.geteuid(),'noNewPrivs':ctypes.CDLL(None).prctl(39,0,0,0,0),
 'seccompMode':int(status['Seccomp']),'filterCount':int(status['Seccomp_filters']),
 'vsockDenied':denied,'fileSizeLimit':list(resource.getrlimit(resource.RLIMIT_FSIZE))}))
"""
    output = subprocess.check_output(['/usr/local/bin/shield-sandbox', '/usr/bin/python3', '-I', '-c', code],
             preexec_fn=partial(desktop_identity, file_limit), timeout=15,
             env={'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'LANG': 'C.UTF-8'})
    probe = json.loads(output)
    require(probe['uid'] == probe['euid'] == 1000 and probe['noNewPrivs'] == 1 and
            probe['seccompMode'] == 2 and probe['filterCount'] > int(parent['Seccomp_filters']) and
            probe['vsockDenied'] is True and probe['fileSizeLimit'] == [file_limit, file_limit],
            'desktop sandbox probe failed')
    return probe


def canary_plan(result, name, variant):
    require(isinstance(result, dict) and set(result) == {name} and isinstance(result[name], dict),
            'installed canary result shape')
    plan = result[name]
    if variant is None:
        valid = (plan.get('schema') == 'shield-opencl-lowering/1' and
                 plan.get('operation') == 'gemm-i8-i8-i64' and plan.get('dispatchable', True) is True and
                 plan.get('inputBound') == 128 and plan.get('dimensions') == 2 and
                 plan.get('arguments') == {'a': 0, 'b': 1, 'c': 2, 'm': 3, 'n': 4, 'k': 5})
    else:
        valid = (plan.get('schema') == 'shield-opencl-q8-analysis/1' and
                 plan.get('operation') == 'q8_0-f32-matmul' and plan.get('dispatchable') is False and
                 plan.get('variant') == variant and plan.get('arguments') ==
                 {'quants': 0, 'scales': 1, 'input': 2, 'inputOffset': 3, 'output': 4, 'outputOffset': 5})
    require(valid, 'installed canary returned an invalid plan')


def canaries(file_limit):
    root = Path(COMPILER).parent
    cases = [{'source': (root / 'gemm_i8.cl').read_text(), 'options': flags}
             for flags in ('', '-cl-std=CL1.2')]
    for name in ('mul_mm_q8_0_f32_l4_lm.cl', 'mul_mv_q8_0_f32_flat.cl'):
        for flags in ('-cl-std=CL3.0', '-cl-std=CL3.0 -cl-mad-enable '
                      '-cl-unsafe-math-optimizations -cl-finite-math-only -cl-fast-relaxed-math'):
            cases.append({'source': (root / 'q8-reference' / name).read_text(), 'options': flags})
    results = []
    for index, case in enumerate(cases):
        variant = None if index < 2 else ('mm' if index < 4 else 'mv')
        name = 'gemm' if variant is None else ('kernel_mul_mm_q8_0_f32_l4_lm' if variant == 'mm' else 'kernel_mul_mv_q8_0_f32_flat')
        value = installed_result(case, file_limit)
        canary_plan(value, name, variant)
        results.append({'fingerprint': fingerprint(case['source'], case['options']), 'kernel': name,
                        'variant': variant, 'resultSha256': hashlib.sha256(json.dumps(value,
                        sort_keys=True, separators=(',', ':')).encode()).hexdigest()})
    return results


def verify_installed(build):
    build = Path(build)
    manifest, receipt = verify_build(build, '/opt/shield/opencl/lib/libOpenCL.so.1')
    for name, digest in manifest['compilerSources'].items():
        require(sha(Path(COMPILER).parent / name) == digest, 'installed compiler source: ' + name)
    env = manifest['environment']
    require(sys.version == env['pythonVersion'] and sha(sys.executable) == env['pythonSha256'],
            'installed Python differs')
    clang = '/usr/bin/clang-18'
    require(sha(clang) == env['clangSha256'], 'installed Clang differs')
    require(subprocess.check_output([clang, '--version'], text=True).strip() == env['clangVersion'],
            'installed Clang version differs')
    resource_dir = Path(subprocess.check_output([clang, '-print-resource-dir'], text=True).strip())
    headers = {str(p.relative_to(resource_dir)): sha(p) for p in sorted(resource_dir.rglob('*')) if p.is_file()}
    require(headers == env['clangHeaders'], 'installed Clang resources differ')
    profiles = []
    for file_limit in (FILE_LIMIT, 32 << 20):
        probe = sandbox_probe(file_limit)
        require(canaries(file_limit) == manifest['canaryResults'], 'installed canaries changed')
        for record in manifest['records']:
            request = json.loads((build / 'requests' / record['requestFile']).read_bytes())
            for _ in range(2):
                result = installed_result(request, file_limit)
                digest = hashlib.sha256(json.dumps(result, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
                require(digest == record['resultSha256'] and (result == {}) == record['declinedBothRuns'],
                        'installed source classification changed: ' + record['requestFile'])
        require(canaries(file_limit) == manifest['canaryResults'], 'installed canaries changed after corpus')
        profiles.append({'probe': probe, 'canariesBeforeAndAfter': 6,
                         'sourceRequestsRecheckedTwice': len(manifest['records'])})
    return {'schema': 'shield-source-declines-installed-check/2', 'passed': True,
            'tableManifestSha256': receipt['tableManifestSha256'], 'profiles': profiles,
            'scope': 'measured sandbox child with explicit gate environment; runtime helper inherits the application sandbox/environment',
            'environmentBindings': ['compiler sources', 'Python binary/version', 'Clang binary/version/resources'],
            'generationEnvironment': 'recorded observations; source results rechecked under both gate profiles'}



if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--installed', type=Path, required=True)
    parser.add_argument('--receipt', type=Path, required=True)
    args = parser.parse_args()
    result = verify_installed(args.installed)
    args.receipt.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))
