"""Run as the ordinary desktop user inside the VM; synthetic files only."""
import ctypes
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile

libc = ctypes.CDLL(None, use_errno=True)
libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
assert fd >= 0
records = []
with tempfile.TemporaryDirectory(prefix='shield-prompt-watch-') as name:
    folder = Path(name)
    folder.chmod(0o755)
    file = folder / 'canary.txt'
    file.write_text('SYNTHETIC_PUBLIC_CANARY_ONLY\n')
    file.chmod(0o644)
    for path in (folder, file):
        assert libc.inotify_add_watch(fd, os.fsencode(path), 0x20) >= 0
    file.read_bytes()
    assert os.read(fd, 65536)
    list(folder.iterdir())
    assert os.read(fd, 65536)
    print('IN_OPEN file and directory positive controls: PASS', flush=True)
    for mode in ('--cpu', '--approximate'):
        for prompt in ('/read ' + str(file), '/glob ' + str(folder / '*.txt')):
            result = subprocess.run(['shield-compute', 'llama', mode, '-p', prompt,
                '--tokens', '8', '--seed', '42'], capture_output=True, text=True, timeout=30)
            assert result.returncode == 0 and ' | Generation:' in result.stdout, result.stderr[-500:]
            assert not select.select([fd], [], [], 0)[0], 'unexpected synthetic file/directory open'
            metadata = json.loads(result.stderr)
            if mode == '--approximate':
                assert metadata['stats']['exchanges'] > 0 and metadata['stats']['verification_failures'] == 0
            record = {'mode': mode, 'prompt': prompt.split(' ')[0], 'generated': True, 'fileOpened': False}
            if mode == '--approximate':
                record['verifiedExchanges'] = metadata['stats']['exchanges']
            records.append(record)
            print(json.dumps(record), flush=True)
os.close(fd)
print('PASS: all 4 requests generated; no synthetic file/directory opened', flush=True)
