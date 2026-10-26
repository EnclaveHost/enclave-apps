#!/usr/bin/env python3
"""Host-only experiment: real Qwen through the standard OpenCL backend.

Run in the current desktop container without network or GPU devices. Mount the
experimental OpenCL plugin at /probe, public models at /models and writable
/evidence. Success here proves local API/kernel compatibility, not masked work.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import re
import signal
import subprocess
import time


def parse_kernel_profile(stdout):
    # Pinned PoCL 6 prints raw event timestamp differences (nanoseconds),
    # although its cq profiler column headers incorrectly say "us".
    return [{'kernel': name, 'launches': int(count), 'totalNanoseconds': int(duration),
             'percentage': int(percentage), 'averageNanoseconds': int(average)}
            for name, count, duration, percentage, average in re.findall(
                r'^\s*\d+\)\s+(\S+)\s+(\d+)\s+(\d+)\s+(\d+)%\s+(\d+)\s*$',
                stdout, re.MULTILINE)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--layers', type=int, choices=(0, 1, 99), required=True)
    parser.add_argument('--timeout', type=int, default=180)
    parser.add_argument('--compiler-debug', action='store_true',
                        help='retain PoCL compiler inputs and detailed compiler diagnostics')
    parser.add_argument('--expected-pocl-sha',
                        help='require this digest at the actual ICD library path before running')
    parser.add_argument('--masked-q8', action='store_true',
                        help='explicitly opt into experimental shield-fixed-point-v1 Q8 service lowering')
    parser.add_argument('--native-cpu-fallback', action='store_true',
                        help='use native CPU for GGML operations outside the experimental masked Q8 profile')
    parser.add_argument('--profile-kernels', action='store_true',
                        help='collect PoCL command-queue kernel names, launch counts and timings')
    args = parser.parse_args()
    if args.layers and not args.expected_pocl_sha:
        parser.error('OpenCL experiments require --expected-pocl-sha to verify the loaded ICD path')
    pocl_sha = hashlib.sha256(Path('/opt/shield/pocl/lib/libpocl.so.2').read_bytes()).hexdigest()
    if args.expected_pocl_sha and pocl_sha != args.expected_pocl_sha:
        parser.error('PoCL ICD library digest does not match the requested experimental runtime')
    resource.setrlimit(resource.RLIMIT_FSIZE, (8 << 20, 8 << 20))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    config = json.loads(Path('/shield/service.json').read_text())
    model = config['models']['llama']
    with open(model['model'], 'rb') as stream:
        assert hashlib.file_digest(stream, 'sha256').hexdigest() == model['modelSha256']
    output = Path('/evidence')
    label = 'cpu' if args.layers == 0 else 'opencl-' + str(args.layers)
    env = {'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'LANG': 'C.UTF-8',
           'GGML_BACKEND_PATH': '/opt/shield/lib/backends/libggml-cpu.so',
           'POCL_MAX_PTHREAD_COUNT': '2', 'POCL_CPU_MAX_CU_COUNT': '2',
           'POCL_CACHE_DIR': '/evidence/pocl-cache'}
    if args.native_cpu_fallback:
        env['GGML_SHIELD_NATIVE_CPU_FALLBACK'] = '1'
    if args.masked_q8:
        env['SHIELD_OPENCL_ARITHMETIC'] = 'shield-fixed-point-v1'
    if args.compiler_debug:
        env.update({'POCL_DEBUG': 'error,warning,general,llvm',
                    'POCL_LEAVE_KERNEL_COMPILER_TEMP_FILES': '1'})
    if args.profile_kernels:
        env['POCL_TRACING'] = 'cq'
    command = ['/opt/shield/bin/llama-cli', '-m', model['model'],
        '--device', 'none' if args.layers == 0 else 'GPUOpenCL', '-ngl', str(args.layers),
        '-p', 'The capital of France is', '-n', '8', '-c', '512', '-b', '32', '-ub', '32',
        '-t', '2', '--temp', '0', '--seed', '42', '--flash-attn', 'off', '--verbosity', '4',
        '--no-conversation', '--single-turn', '--no-warmup', '--no-display-prompt', '--color', 'off']
    if args.native_cpu_fallback:
        # SET_ROWS mutates the persistent KV cache. Keep that allocation on CPU
        # because the restricted Shield backend only executes eligible Q8 matmul.
        command.append('--no-kv-offload')
    start = time.monotonic()
    timed_out = False
    with (output / (label + '.stdout')).open('wb') as stdout, (output / (label + '.stderr')).open('wb') as stderr:
        process = subprocess.Popen(command, cwd='/probe', env=env, stdin=subprocess.DEVNULL,
                                   stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            process.wait(timeout=args.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    stdout = (output / (label + '.stdout')).read_text(errors='replace')
    stderr = (output / (label + '.stderr')).read_text(errors='replace')
    record = {'schema': 'shield-native-opencl-application-probe/1',
        'scope': ('host container with dedicated Shield worker; masked physical GPU route; not confidential VM evidence'
                  if args.masked_q8 else 'host container with no network/GPU; local API compatibility only'),
        'requestedGpuLayers': args.layers,
        'q8Arithmetic': 'shield-fixed-point-v1' if args.masked_q8 else 'original-opencl-kernels', 'command': command, 'environment': env,
        'exitCode': process.returncode, 'timedOut': timed_out, 'seconds': time.monotonic() - start,
        'modelSha256': model['modelSha256'],
        'pluginSha256': hashlib.sha256(Path('/probe/libggml-opencl.so').read_bytes()).hexdigest(),
        'poclLibrarySha256': pocl_sha,
        'frontendLibrarySha256': hashlib.sha256(Path('/opt/shield/opencl/lib/libOpenCL.so.1').read_bytes()).hexdigest(),
        'kernelLibrarySha256': hashlib.sha256(Path('/opt/shield/pocl/share/pocl/kernel-x86_64-pc-linux-gnu-x86-64.bc').read_bytes()).hexdigest(),
        'generated': ' | Generation:' in stdout, 'containsParis': 'Paris' in stdout,
        'kernelProfile': parse_kernel_profile(stdout),
        'kernelProfileUnits': 'nanoseconds; pinned PoCL 6 cq column headers incorrectly say us',
        'openclDiagnostics': [line[:1000] for line in stderr.splitlines()
                              if any(marker in line for marker in
                                     ('ggml_opencl:', 'offload', 'failed', 'LLVM ERROR',
                                      'undefined symbol', 'Assertion'))][-40:]}
    (output / (label + '.json')).write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps(record), flush=True)
    if process.returncode or timed_out or not record['generated']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
