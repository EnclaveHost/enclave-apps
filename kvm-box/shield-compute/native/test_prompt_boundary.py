#!/usr/bin/env python3
"""Container integration: real Qwen CLI via service socket and direct -p.

Mount public models at /models and writable /evidence. Uses only the dedicated
worker on loopback 9741. inotify observes actual opens of synthetic files and
directories; generated text is not used to infer whether a file was opened.
"""
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import resource
import signal
import subprocess
import sys
import tempfile
import time

from test_file_limit import call


def main():
    # Bound regression output even if the direct CLI returns to its former loop.
    resource.setrlimit(resource.RLIMIT_FSIZE, (8 << 20, 8 << 20))
    spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
    service = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(service)
    config = json.loads(Path('/shield/service.json').read_text())
    config.update(worker={'host': '127.0.0.1', 'port': 9741},
                  jobs='/tmp', socket='/tmp/prompt-test.sock', clientUids=[os.getuid()], timeoutSeconds=60)
    config_path = Path('/tmp/prompt-test-config.json')
    config_path.write_text(json.dumps(config))
    records = []
    libc = ctypes.CDLL(None, use_errno=True)
    libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
    fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
    assert fd >= 0
    with tempfile.TemporaryDirectory(prefix='shield-prompt-canary-') as directory:
        folder = Path(directory)
        canary = folder / 'canary.txt'
        canary.write_text('SYNTHETIC_PROMPT_BOUNDARY_CANARY_ONLY\n')
        for path in (folder, canary):
            assert libc.inotify_add_watch(fd, os.fsencode(path), 0x20) >= 0  # IN_OPEN
        def opened():
            try:
                return os.read(fd, 65536)
            except BlockingIOError:
                return b''
        # Prove the watcher can observe both file and directory access.
        canary.read_bytes()
        assert opened(), 'file-open positive control missing'
        list(folder.iterdir())
        assert opened(), 'directory-open positive control missing'
        with Path('/evidence/service.log').open('wb') as log:
            server = subprocess.Popen([sys.executable, '/shield/service.py', '--config', str(config_path)],
                                      stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 30
                while not Path(config['socket']).exists():
                    assert server.poll() is None
                    if time.monotonic() > deadline:
                        raise TimeoutError('service startup')
                    time.sleep(.05)
                assert resource.prlimit(server.pid, resource.RLIMIT_FSIZE) == (8 << 20, 8 << 20)
                prompts = ['/read ' + str(canary), '/glob ' + str(folder / '*.txt'),
                           '/exit', '/clear', '/regen', 'The capital of France is']
                cases = [('cpu', p) for p in prompts] + [('masked', p) for p in prompts[:2]]
                for mode, prompt in cases:
                    started = time.monotonic()
                    result = call(config['socket'], {'op': 'llama', 'mode': mode, 'prompt': prompt,
                        'tokens': 8, 'seed': 42,
                        'arithmetic': service.FIXED_ARITHMETIC if mode == 'masked' else service.NATIVE_ARITHMETIC})
                    assert result.get('ok') and ' | Generation:' in result.get('text', ''), result
                    assert not opened(), 'prompt triggered a synthetic file/directory open'
                    if mode == 'masked':
                        assert result['stats']['exchanges'] > 0 and result['stats']['verification_failures'] == 0
                    assert len(result['text'].encode()) < 65536
                    records.append({'mode': mode, 'promptClass': prompt.split(' ')[0],
                        'seconds': time.monotonic() - started, 'syntheticFileOpened': False,
                        'textBytes': len(result['text'].encode()), 'stats': result['stats']})
                status = call(config['socket'], {'op': 'status'})
                assert status['state'] == 'ready' and status['completed'] == len(cases), status
                # Service uses -f. Also exercise direct -p against the real CLI.
                compute = service.Compute(config)
                env = compute.environment(config['models']['llama'], False, '/tmp', 'llama')
                args = [config['sdk'] + '/bin/llama-cli', '-m', config['models']['llama']['model'],
                    '-n', '8', '-c', '1024', '-b', '32', '-ub', '32', '-t', '2',
                    '--temp', '0', '--no-conversation', '--single-turn', '--no-warmup',
                    '--no-display-prompt', '--color', 'off']
                def direct_cli(name, command, input_bytes=b''):
                    input_file = Path('/tmp/direct-cli-input')
                    input_file.write_bytes(input_bytes)
                    output_file = Path('/evidence/' + name + '.stdout')
                    error_file = Path('/evidence/' + name + '.stderr')
                    with input_file.open('rb') as stdin, output_file.open('wb') as stdout, error_file.open('wb') as stderr:
                        process = subprocess.Popen(command, env=env, stdin=stdin, stdout=stdout,
                                                   stderr=stderr, start_new_session=True)
                        try:
                            process.wait(timeout=15)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGKILL)
                            process.wait()
                            raise
                    assert process.returncode == 0, error_file.read_bytes()[-2000:]
                    return output_file.read_bytes()
                direct = direct_cli('direct-argument', args + ['-p', prompts[0]])
                assert b' | Generation:' in direct and not opened()
                interactive = direct_cli('interactive-exit', args, b'/exit\n')
                assert b' | Generation:' not in interactive and not opened()
                receipt = {'schema': 'shield-literal-prompts/1',
                    'scope': 'host container, actual service process/socket and packaged Qwen CLI',
                    'inotifyPositiveControls': ['file', 'directory'], 'cases': records,
                    'directArgumentReadIsLiteral': True, 'interactiveExitStillWorks': True, 'status': status}
                Path('/evidence/prompt-boundary.json').write_text(json.dumps(receipt, indent=2) + '\n')
                print(json.dumps(receipt), flush=True)
            finally:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()
                os.close(fd)


if __name__ == '__main__':
    main()
