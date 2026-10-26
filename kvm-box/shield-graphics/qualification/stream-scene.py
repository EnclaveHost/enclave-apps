import hashlib, json, os, signal, subprocess, time
from pathlib import Path
assert os.getuid() == 1000
assert not Path('/dev/dri').exists() and not Path('/dev/nvidia0').exists()
env = {**os.environ, **json.loads(Path('/tmp/probe-graphics-environment.json').read_text()),
       'DISPLAY': ':0', 'XAUTHORITY': '/run/user/1000/Xauthority',
       'XDG_RUNTIME_DIR': '/run/user/1000', 'HOME': '/home/desktop'}
procs = []
def terminate(*_): raise SystemExit(0)
signal.signal(signal.SIGTERM, terminate)
try:
    for name, args in [('cube', ['vkcube', '--width', '480', '--height', '360']),
                       ('gears', ['glxgears', '-geometry', '480x360+600+150'])]:
        log = open('/tmp/stream-'+name+'.log', 'w')
        p = subprocess.Popen(args, env=env, stdout=log, stderr=subprocess.STDOUT)
        procs.append(p)
    time.sleep(3)
    identities = {}
    for p in procs:
        assert p.poll() is None
        maps = Path(f'/proc/{p.pid}/maps').read_text()
        identities[str(p.pid)] = {}
        for name in ['libvulkan_lvp.so', 'libgallium-25.0.7.so']:
            lib = Path('/opt/shield/mesa/lib') / name
            if str(lib) in maps: identities[str(p.pid)][name] = hashlib.sha256(lib.read_bytes()).hexdigest()
        assert identities[str(p.pid)].get('libvulkan_lvp.so')
    Path('/tmp/stream-scene-ready.json').write_text(json.dumps({'loadedLibraries': identities}))
    for _ in range(180):
        if Path('/tmp/stream-scene-stop').exists(): break
        assert all(p.poll() is None for p in procs)
        time.sleep(1)
finally:
    for p in procs:
        if p.poll() is None: p.terminate()
    for p in procs:
        try: p.wait(5)
        except subprocess.TimeoutExpired: p.kill(); p.wait(3)
