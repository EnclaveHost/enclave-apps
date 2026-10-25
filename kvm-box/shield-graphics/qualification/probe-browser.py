#!/usr/bin/env python3
"""Run a local-file WebGL workload in the real desktop browser, with no test IP listener."""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time

assert os.getuid() == 1000
assert not Path('/dev/dri').exists() and not Path('/dev/nvidia0').exists()
env = {**os.environ, 'DISPLAY': ':0', 'XAUTHORITY': '/run/user/1000/Xauthority',
       'XDG_RUNTIME_DIR': '/run/user/1000', 'HOME': '/home/desktop'}
evidence = Path('/tmp/graphics-evidence')
profile = Path('/home/desktop/.mozilla/firefox/kvm-box/user.js')
profile_hash = hashlib.sha256(profile.read_bytes()).hexdigest()
result = None
last_titles = {}
chunks = {}
message_id = None
message_count = None
with (evidence / 'browser.log').open('w') as log:
    browser = subprocess.Popen(['/usr/local/bin/risc-browser', 'file:///tmp/browser-webgl.html'],
                               env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            assert browser.poll() is None, ('browser exited', browser.returncode)
            windows = subprocess.run(['/usr/local/bin/risc-xwin-native', 'list'], env=env,
                                     capture_output=True, text=True, timeout=3)
            for window in windows.stdout.splitlines():
                assert re.fullmatch(r'0x[0-9a-f]+', window)
                title = subprocess.run(['xprop', '-id', window, '_NET_WM_NAME'], env=env,
                                       capture_output=True, text=True, timeout=3).stdout
                last_titles[window] = title
                match = re.search(r'SHIELD_WGL:([0-9a-f]{8}):([0-9]+):([0-9]+):([A-Za-z0-9+/=]{1,160}):END', title)
                if match:
                    nonce, index, count, payload = match.groups()
                    index, count = int(index), int(count)
                    assert 1 <= count <= 100 and 0 <= index < count
                    if message_id is None:
                        message_id, message_count = nonce, count
                    assert (nonce, count) == (message_id, message_count), 'mixed browser reports'
                    assert index not in chunks or chunks[index] == payload, 'inconsistent report chunk'
                    chunks[index] = payload
                    if len(chunks) == count:
                        encoded = ''.join(chunks[i] for i in range(count))
                        result = json.loads(base64.b64decode(encoded, validate=True))
                        break
            if result is not None:
                break
            time.sleep(.2)
        assert result is not None, 'WebGL result deadline'
        result['browserProfileSha256'] = profile_hash
        assert hashlib.sha256(profile.read_bytes()).hexdigest() == profile_hash
        (evidence / 'browser-result.json').write_text(json.dumps(result, indent=2) + '\n')
        pending = [browser.pid]
        unreadable = []
        maps = {}
        environments = []
        while pending:
            pid = pending.pop()
            try:
                pending.extend(int(x) for x in Path(f'/proc/{pid}/task/{pid}/children').read_text().split())
                mapping = Path(f'/proc/{pid}/maps').read_text()
                public = dict(x.split('=', 1) for x in Path(f'/proc/{pid}/environ').read_bytes().decode().split('\0') if '=' in x)
                environments.append({k: public.get(k) for k in ['GALLIUM_DRIVER', 'LP_NUM_THREADS', 'LIBGL_DRIVERS_PATH', 'LD_LIBRARY_PATH']})
                for name in ['libvulkan_lvp.so', 'libgallium-25.0.7.so']:
                    path = Path('/opt/shield/mesa/lib') / name
                    if str(path) in mapping:
                        maps[name] = hashlib.sha256(path.read_bytes()).hexdigest()
            except PermissionError:
                unreadable.append(pid)
            except (FileNotFoundError, ProcessLookupError):
                pass
        result['unreadableProcesses'] = unreadable
        result['loadedLibraries'] = maps
        result['graphicsEnvironments'] = environments
        capture = subprocess.run(['python3', '/tmp/capture-reference.py', str(evidence / 'browser-webgl.png')],
                                 env=env, capture_output=True, text=True, timeout=8)
        assert capture.returncode == 0, capture.stderr
        (evidence / 'browser-result.json').write_text(json.dumps(result, indent=2) + '\n')
        assert result.get('passed'), result
        assert len(result['results']) == 2 and all(r['passed'] and r['channelsChecked'] == 65536 for r in result['results'])
        assert set(maps) == {'libvulkan_lvp.so', 'libgallium-25.0.7.so'}, maps
        assert all(e['GALLIUM_DRIVER'] == 'zink' and e['LP_NUM_THREADS'] == '1' for e in environments)
        print(json.dumps(result))
    finally:
        (evidence / 'browser-windows.json').write_text(json.dumps(last_titles, indent=2) + '\n')
        if result is None:
            print('BROWSER_LAST_TITLES='+json.dumps(last_titles), file=__import__('sys').stderr)
            subprocess.run(['python3','/tmp/capture-reference.py',str(evidence/'browser-timeout.png')],env=env,capture_output=True,timeout=8)
        if browser.poll() is None:
            os.killpg(browser.pid, signal.SIGTERM)
            try:
                browser.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(browser.pid, signal.SIGKILL)
                browser.wait(timeout=5)
