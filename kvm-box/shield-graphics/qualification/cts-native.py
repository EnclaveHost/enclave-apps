"""Run public CTS as the actual isolated desktop UID on its existing Xorg display."""
import atexit
import sys
import base64
from collections import Counter
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import socket
import struct
import tarfile

assets = Path('/tmp/native-cts-input')
output = Path('/tmp/native-cts-output')
assert os.getuid() == os.geteuid() == 1000
assert os.uname().release == '6.6.87.2-enclave-kvm-box'
status = dict(line.split(':', 1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
assert all(int(status[key].strip(), 16) == 0 for key in ['CapEff', 'CapPrm', 'CapBnd', 'CapAmb'])
assert status['NoNewPrivs'].strip() == '1' and status['Seccomp'].strip() == '2'
assert not Path('/dev/dri').exists() and not Path('/dev/kfd').exists() and not list(Path('/dev').glob('nvidia*'))
assert os.environ['DISPLAY'] == ':0' and os.environ['GALLIUM_DRIVER'] == 'zink'
assert os.environ['ZINK_DESCRIPTORS'] == 'lazy'

def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result

probe = module('cts_probe', assets/'qualification/cts-probe.py')
result_rules = module('cts_results', assets/'qualification/cts-results.py')
evaluate = result_rules.evaluate
lock = json.loads((assets/'qualification/cts-lock.json').read_text())
api = (assets/'native-api').read_text().strip() if (assets/'native-api').exists() else 'gl'
assert api in ('gl', 'vk')
gl_context = (assets/'native-gl-context').read_text().strip() if (assets/'native-gl-context').exists() else 'egl'
assert gl_context in ('egl', 'glx')
plan = json.loads((assets/'qualification/cts-plan.json').read_text())[api]
expected_status = json.loads((assets/'expected-results.json').read_text())
required = ['/opt/shield/mesa/lib/'+name for name in lock['mesa']] if api == 'gl' else ['/opt/shield/mesa/lib/libvulkan_lvp.so']
for name, digest in lock['mesa'].items():
    assert probe.sha('/opt/shield/mesa/lib/'+name) == digest
loader_identity = {}
if api == 'gl':
    name = 'libEGL_mesa.so.0.0.0' if gl_context == 'egl' else 'libGLX_mesa.so.0.0.0'
    loader = '/opt/shield/mesa/lib/'+name
    assert probe.sha(loader) == lock['loaders'][name]
    required.append(loader)
    loader_identity[name] = lock['loaders'][name]
parser_path = assets/'source/scripts/log/log_parser.py'
assert probe.sha(parser_path) == lock['qpaParserSha256']
parser = module('qpa_parser', parser_path)
binary = assets/('build/external/openglcts/modules/glcts' if api == 'gl' else 'build/external/vulkancts/modules/vulkan/deqp-vk')
assert probe.sha(binary) == {
    'gl': '33979163a00d6eb79d20950ae14a9ab89bf5c3bc84bd53b66aece9bca3ae2079',
    'vk': '2bb9cb3260fa3cb35c9c67845f098115a89a9522391b4a3c4063ef261805fb82',
}[api]
output.mkdir(mode=0o700)
finished = False

def retain_failure():
    if finished:
        return
    try:
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode='w') as archive:
            for path in sorted(output.rglob('*')):
                if path.is_file():
                    archive.add(path, arcname=str(path.relative_to(output)), recursive=False)
        compressed = gzip.compress(data.getvalue(), mtime=0)
        if len(compressed) < 4*1024*1024:
            print('NATIVE_CTS_FAILURE_ARCHIVE='+base64.b64encode(compressed).decode(), file=sys.stderr)
    except Exception as error:
        print('NATIVE_CTS_FAILURE_ARCHIVE_ERROR='+type(error).__name__, file=sys.stderr)
atexit.register(retain_failure)

def compute_status():
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(5)
        s.connect('/run/shield/compute.sock')
        assert struct.unpack('3i', s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[1] == 1001
        body = b'{"op":"status"}'
        s.sendall(struct.pack('!I', len(body))+body)
        def exact(n):
            data = bytearray()
            while len(data) < n:
                part = s.recv(n-len(data))
                assert part
                data.extend(part)
            return bytes(data)
        length, = struct.unpack('!I', exact(4))
        assert 0 < length < 65536
        value = json.loads(exact(length))
        assert value['ok'] and value['state'] == 'ready'
        return value

before = compute_status()
environment = {key: os.environ[key] for key in [
    'PATH', 'HOME', 'DISPLAY', 'XAUTHORITY', 'XDG_RUNTIME_DIR', 'VK_DRIVER_FILES',
    'LP_NUM_THREADS', 'LIBGL_DRIVERS_PATH', 'LD_LIBRARY_PATH', 'LIBGL_ALWAYS_SOFTWARE',
    'GALLIUM_DRIVER', 'MESA_LOADER_DRIVER_OVERRIDE', 'LIBGL_KOPPER_DRI2', 'ZINK_DESCRIPTORS']}
assert environment['LP_NUM_THREADS'] == '2'
linker = Path('/tmp/native-cts-linker')
linker.mkdir(mode=0o700)
aliases = {}
for name, soname, digest in ([
    ('libEGL.so', 'libEGL.so.1', 'f4b60a4618fbf760f395f64dfd53320446ba1011151a80e411742b3232b194e6'),
    ('libGL.so', 'libGL.so.1', 'b01915dfedf03be4710d3184bc62a3364880fc950043742fd5e07659b409ef1e'),
    ('libGLESv2.so', 'libGLESv2.so.2', 'a705c66de30ca4e883120c70acae0f1011d7c3f97605eef331e03493beae3812')] if api == 'gl' else []):
    target = Path('/usr/lib/x86_64-linux-gnu')/soname
    assert probe.sha(target) == digest
    (linker/name).symlink_to(target)
    aliases[name] = {'target': str(target), 'sha256': digest}
environment['LD_LIBRARY_PATH'] = str(linker)+':'+environment['LD_LIBRARY_PATH']
groups = {}
records = []
for name, pattern in plan.items():
    folder = output/name
    folder.mkdir()
    options = [str(binary), '--deqp-case='+pattern,
               '--deqp-shadercache=disable', '--deqp-surface-width=256', '--deqp-surface-height=256']
    if api == 'gl':
        options += ['--deqp-gl-context-type='+gl_context]
    listing = probe.run(options+['--deqp-runmode=stdout-caselist',
                        '--deqp-log-filename='+str(folder/'listing.qpa')],
                        binary.parent, environment, folder/'listing.log', 30)
    expected = [line[6:].strip() for line in (folder/'listing.log').read_text().splitlines() if line.startswith('TEST: ')]
    assert listing['returncode'] == 0 and not listing['timedOut'] and expected, (name, listing, (folder/'listing.log').read_text()[-2000:])
    execution = probe.run(options+['--deqp-log-filename='+str(folder/'results.qpa'), '--deqp-watchdog=enable'],
                          binary.parent, environment, folder/'stdout.log', 900 if api == 'vk' else 120)
    qpa = parser.BatchResultParser()
    items = [{'name': r.name, 'status': r.statusCode, 'details': r.statusDetails}
             for r in qpa.parseFile(str(folder/'results.qpa'))]
    verdict = evaluate(expected, items, execution['returncode'], execution['timedOut'],
                       qpa.curCaseName is not None, execution['mappedLibraries'], required)
    report = {'listing': listing, 'execution': execution, 'verdict': verdict, 'results': items}
    (folder/'result.json').write_text(json.dumps(report, indent=2)+'\n')
    groups[name] = verdict
    records.extend(items)
after = compute_status()
assert before['telemetry'] == after['telemetry'], 'Unexpected compute telemetry change during CPU CTS'
assert all(g['complete'] and not g['missingLibraries'] for g in groups.values()), groups
assert len(records) == len(expected_status) == (551 if api == 'gl' else 9300)
assert {r['name'] for r in records} == set(expected_status)
if api == 'gl':
    reference_matches = result_rules.matches_reference(expected_status, records, {
        'KHR-GL45.direct_state_access.framebuffers_check_status'
    })
else:
    reviewed = json.loads((assets/'reviewed-unsupported.json').read_text())
    reference_matches = result_rules.matches_optional_reference(expected_status, records, reviewed)
assert reference_matches, 'Native case statuses differ from the reviewed reference; inspect retained per-case logs'
counts = dict(Counter(r['status'] for r in records))
summary = {'passed': True, 'scope': 'Native desktop role, actual Xorg, immutable candidate root; selected CTS CPU compatibility and unchanged masked-worker counters; not conformance certification or owner release',
           'uid': os.getuid(), 'capabilitiesZero': True, 'noNewPrivs': True, 'seccomp': 2,
           'noPhysicalGpu': True, 'display': ':0', 'api': api, 'context': gl_context.upper() if api == 'gl' else 'Vulkan', 'gpuExecuted': False,
           'counts': counts, 'allSelectedPassed': all(r['status'] == 'Pass' for r in records),
           'matchesContainerBaselineOrImproves': True, 'groups': groups,
           'workerTelemetryUnchanged': True, 'environment': environment, 'testLinkerAliases': aliases,
           'ctsCommit': lock['source']['commit'], 'binarySha256': probe.sha(binary), 'mesa': lock['mesa'], 'loaderIdentity': loader_identity}
(output/'result.json').write_text(json.dumps(summary, indent=2)+'\n')
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w') as archive:
    for path in sorted(output.rglob('*')):
        if path.is_file():
            archive.add(path, arcname=str(path.relative_to(output)), recursive=False)
compressed = gzip.compress(buffer.getvalue(), mtime=0)
assert len(compressed) < 4*1024*1024
summary['evidenceTarGzipBase64'] = base64.b64encode(compressed).decode()
summary['evidenceSha256'] = hashlib.sha256(compressed).hexdigest()
finished = True
print(json.dumps(summary))
