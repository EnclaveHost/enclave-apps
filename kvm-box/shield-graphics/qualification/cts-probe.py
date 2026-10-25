#!/usr/bin/env python3
"""Run a fixed CTS subset inside the isolated qualification container."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import time


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def run(command, cwd, env, log, timeout):
    mapped = set()
    timed_out = False
    start = time.monotonic()
    with log.open('x') as output:
        child = subprocess.Popen(command, cwd=cwd, env=env, stdout=output,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while child.poll() is None:
                try:
                    for line in Path(f'/proc/{child.pid}/maps').read_text().splitlines():
                        fields = line.split(None, 5)
                        if len(fields) == 6 and fields[5].startswith('/opt/shield/mesa/'):
                            mapped.add(fields[5])
                except FileNotFoundError:
                    pass
                if time.monotonic() - start > timeout:
                    timed_out = True
                    os.killpg(child.pid, signal.SIGKILL)
                    break
                time.sleep(.025)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            child.wait()
    return {'command': command, 'returncode': child.returncode, 'timedOut': timed_out,
            'seconds': time.monotonic() - start, 'mappedLibraries': sorted(mapped)}


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--api', choices=['gl', 'vk'], required=True)
    p.add_argument('--group', action='append', default=[])
    p.add_argument('--gl-context', choices=['egl', 'glx'], default='egl')
    a = p.parse_args()
    here = Path(__file__).resolve().parent
    lock = json.loads((here/'cts-lock.json').read_text())
    plan = json.loads((here/'cts-plan.json').read_text())[a.api]
    if a.group:
        if set(a.group) - set(plan):
            raise ValueError('Unknown group')
        plan = {key: value for key, value in plan.items() if key in a.group}
    if Path('/dev/dri').exists() or list(Path('/dev').glob('nvidia*')) or Path('/dev/kfd').exists():
        raise RuntimeError('Qualification must not expose physical GPU devices')
    if Path('/run/shield/compute.sock').exists():
        raise RuntimeError('This CTS pass tests trusted CPU fallback without a compute service')
    libs = {'/opt/shield/mesa/lib/'+name: digest for name, digest in lock['mesa'].items()}
    for path, digest in libs.items():
        if sha(path) != digest:
            raise RuntimeError('Mesa hash mismatch: '+path)
    loader = None
    if a.api == 'gl':
        name = 'libEGL_mesa.so.0.0.0' if a.gl_context == 'egl' else 'libGLX_mesa.so.0.0.0'
        loader = '/opt/shield/mesa/lib/'+name
        libs[loader] = lock['loaders'][name]
        if sha(loader) != libs[loader]:
            raise RuntimeError('Mesa loader hash mismatch: '+loader)
    icd = Path('/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json')
    if json.loads(icd.read_text())['ICD']['library_path'] != '/opt/shield/mesa/lib/libvulkan_lvp.so':
        raise RuntimeError('Unexpected ICD path')
    binary = Path('/cts/build/external/openglcts/modules/glcts' if a.api == 'gl'
                  else '/cts/build/external/vulkancts/modules/vulkan/deqp-vk')
    parser_path = Path('/cts/source/scripts/log/log_parser.py')
    if sha(parser_path) != lock['qpaParserSha256']:
        raise RuntimeError('Unexpected QPA parser')
    parser = module('upstream_cts_log', parser_path)
    evaluate = module('cts_results', here/'cts-results.py').evaluate
    output = Path('/output')
    required = list(libs) if a.api == 'gl' else ['/opt/shield/mesa/lib/libvulkan_lvp.so']
    env = {'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'XDG_CACHE_HOME': '/tmp/cache',
           'VK_DRIVER_FILES': str(icd), 'LP_NUM_THREADS': '1', 'MESA_SHADER_CACHE_DISABLE': 'true'}
    if a.api == 'gl':
        env.update({'LIBGL_DRIVERS_PATH': '/opt/shield/mesa/lib/dri',
                    'LD_LIBRARY_PATH': '/opt/shield/mesa/lib',
                    'LIBGL_ALWAYS_SOFTWARE': '1', 'GALLIUM_DRIVER': 'zink',
                    'MESA_LOADER_DRIVER_OVERRIDE': 'zink', 'LIBGL_KOPPER_DRI2': 'true',
                    'ZINK_DESCRIPTORS': 'lazy'})
    summary = {'api': a.api, 'scope': 'Selected CTS compatibility tests; CPU fallback; not conformance certification or protected GPU execution',
               'binarySha256': sha(binary), 'mesa': libs, 'groups': {}, 'environment': env,
               'source': lock['source'], 'toolsImage': lock['toolsImage'],
               'glContext': a.gl_context if a.api == 'gl' else None,
               'selectedGroups': list(plan), 'completePlan': False, 'allSelectedPassed': False}
    xserver = None
    with tempfile.TemporaryDirectory(prefix='cts-') as runtime:
        env['XDG_RUNTIME_DIR'] = runtime
        try:
            if a.api == 'gl':
                reader, writer = os.pipe()
                try:
                    with (output/'xvfb.log').open('x') as log:
                        xserver = subprocess.Popen(['Xvfb', '-displayfd', str(writer), '-screen', '0',
                                                    '1280x800x24', '-nolisten', 'tcp'],
                                                   pass_fds=(writer,), stdout=log, stderr=log)
                    os.close(writer)
                    writer = None
                    if not select.select([reader], [], [], 15)[0]:
                        raise RuntimeError('Xvfb did not become ready')
                    number = os.read(reader, 32).decode().strip()
                    if not number.isdecimal():
                        raise RuntimeError('Invalid Xvfb display')
                    env['DISPLAY'] = ':'+number
                finally:
                    os.close(reader)
                    if writer is not None:
                        os.close(writer)
            for name, pattern in plan.items():
                logdir = output/name
                logdir.mkdir()
                options = [str(binary), '--deqp-case='+pattern, '--deqp-shadercache=disable',
                           '--deqp-surface-width=256', '--deqp-surface-height=256']
                if a.api == 'gl':
                    options += ['--deqp-gl-context-type='+a.gl_context]
                listing = run(options + ['--deqp-runmode=stdout-caselist',
                              '--deqp-log-filename='+str(logdir/'listing.qpa')],
                              binary.parent, env, logdir/'listing.log', 120)
                expected = [line[6:].strip() for line in (logdir/'listing.log').read_text().splitlines()
                            if line.startswith('TEST: ')]
                (logdir/'cases.json').write_text(json.dumps(expected, indent=2)+'\n')
                if listing['returncode'] or listing['timedOut'] or not expected:
                    raise RuntimeError('Could not enumerate '+name)
                execution = run(options + ['--deqp-log-filename='+str(logdir/'results.qpa'),
                                '--deqp-watchdog=enable'], binary.parent, env, logdir/'stdout.log', 900)
                qpa = parser.BatchResultParser()
                results = qpa.parseFile(str(logdir/'results.qpa'))
                records = [{'name': r.name, 'status': r.statusCode, 'details': r.statusDetails} for r in results]
                verdict = evaluate(expected, records, execution['returncode'], execution['timedOut'],
                                   qpa.curCaseName is not None, execution['mappedLibraries'], required)
                report = {'pattern': pattern, 'listing': listing, 'execution': execution,
                          'verdict': verdict, 'results': records}
                (logdir/'result.json').write_text(json.dumps(report, indent=2)+'\n')
                summary['groups'][name] = verdict
                (output/'result.json').write_text(json.dumps(summary, indent=2)+'\n')
                print(json.dumps({'group': name, **verdict}), flush=True)
        except Exception as error:
            summary['error'] = {'type': type(error).__name__, 'message': str(error)}
            (output/'result.json').write_text(json.dumps(summary, indent=2)+'\n')
            raise
        finally:
            if xserver is not None:
                xserver.terminate()
                xserver.wait(timeout=10)
    summary['completePlan'] = set(summary['groups']) == set(plan)
    summary['allSelectedPassed'] = summary['completePlan'] and bool(summary['groups']) and all(
        g['allSelectedPassed'] for g in summary['groups'].values())
    (output/'result.json').write_text(json.dumps(summary, indent=2)+'\n')
    return 0 if summary['allSelectedPassed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
