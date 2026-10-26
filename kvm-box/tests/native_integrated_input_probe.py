"""Guest-only physical input-path check; no XTest or synthetic X events."""
import ctypes as C
import fcntl
import json
import os
from pathlib import Path
import select
import stat
import struct
import time

if os.uname().release != '6.6.87.2-enclave-kvm-box' or os.getuid() != 1000:
    raise SystemExit('requires isolated native-input probe guest and desktop UID')
if Path('/run/kvm-box-native-input-probe').read_text() != 'INPUT-PROBE/1\n':
    raise SystemExit('missing guest probe marker')
if Path('/proc/1').exists():
    raise SystemExit('private root must hide root-owned init from desktop UID')

X = C.CDLL('libX11.so.6')
Display = C.c_void_p
Window = C.c_ulong
for name, result, args in [
    ('XOpenDisplay', Display, [C.c_char_p]),
    ('XDefaultRootWindow', Window, [Display]),
    ('XCreateSimpleWindow', Window, [Display, Window, C.c_int, C.c_int, C.c_uint, C.c_uint, C.c_uint, C.c_ulong, C.c_ulong]),
    ('XSelectInput', C.c_int, [Display, Window, C.c_long]),
    ('XChangeWindowAttributes', C.c_int, [Display, Window, C.c_ulong, C.c_void_p]),
    ('XMapWindow', C.c_int, [Display, Window]),
    ('XSetInputFocus', C.c_int, [Display, Window, C.c_int, C.c_ulong]),
    ('XSync', C.c_int, [Display, C.c_int]),
    ('XPending', C.c_int, [Display]),
    ('XConnectionNumber', C.c_int, [Display]),
    ('XNextEvent', C.c_int, [Display, C.c_void_p]),
    ('XLookupKeysym', C.c_ulong, [C.c_void_p, C.c_int]),
    ('XCloseDisplay', C.c_int, [Display]),
]:
    fn = getattr(X, name); fn.restype = result; fn.argtypes = args

class WindowAttributes(C.Structure):
    _fields_ = [(n, t) for n, t in [
        ('background_pixmap', C.c_ulong), ('background_pixel', C.c_ulong),
        ('border_pixmap', C.c_ulong), ('border_pixel', C.c_ulong),
        ('bit_gravity', C.c_int), ('win_gravity', C.c_int), ('backing_store', C.c_int),
        ('backing_planes', C.c_ulong), ('backing_pixel', C.c_ulong),
        ('save_under', C.c_int), ('event_mask', C.c_long), ('do_not_propagate_mask', C.c_long),
        ('override_redirect', C.c_int), ('colormap', C.c_ulong), ('cursor', C.c_ulong)]]

class KeyEvent(C.Structure):
    _fields_ = [('type', C.c_int), ('serial', C.c_ulong), ('send_event', C.c_int),
               ('display', Display), ('window', Window), ('root', Window),
               ('subwindow', Window), ('time', C.c_ulong), ('x', C.c_int),
               ('y', C.c_int), ('x_root', C.c_int), ('y_root', C.c_int),
               ('state', C.c_uint), ('keycode', C.c_uint), ('same_screen', C.c_int)]

class Event(C.Union):
    _fields_ = [('key', KeyEvent), ('pad', C.c_long * 24)]

display = X.XOpenDisplay(b':0')
if not display:
    raise RuntimeError('Xorg unavailable')
keyboard = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK | os.O_CLOEXEC)
created = False
try:
    # Linux uinput ioctl values for the fixed x86_64 guest ABI.
    fcntl.ioctl(keyboard, 0x40045564, 1)  # UI_SET_EVBIT EV_KEY
    fcntl.ioctl(keyboard, 0x40045564, 20)  # EV_REP
    for key in range(1, 256):
        fcntl.ioctl(keyboard, 0x40045565, key)
    name = b'KVMBox Native Input Probe'
    setup = struct.pack('HHHH80sI', 3, 0x1209, 1, 1, name, 0)
    fcntl.ioctl(keyboard, 0x405c5503, setup)  # UI_DEV_SETUP
    fcntl.ioctl(keyboard, 0x5501)  # UI_DEV_CREATE
    created = True
    deadline = time.monotonic() + 12
    device = None
    while time.monotonic() < deadline:
        for path in Path('/sys/devices/virtual/input').glob('input*/name'):
            if path.read_bytes().strip() == name:
                for event in path.parent.glob('event*/dev'):
                    major, minor = map(int, event.read_text().strip().split(':'))
                    node = Path('/dev/input') / event.parent.name
                    database = Path(f'/run/udev/data/c{major}:{minor}')
                    if node.exists() and database.exists() and node.stat().st_uid == 1000:
                        properties = database.read_text()
                        if 'E:ID_INPUT_KEYBOARD=1\n' in properties:
                            device = str(node)
        if device:
            break
        time.sleep(.05)
    if not device:
        raise RuntimeError('guest udev did not initialize keyboard or assign its owner')
    metadata = os.stat(device)
    if metadata.st_uid != 1000 or metadata.st_gid != 1000 or stat.S_IMODE(metadata.st_mode) != 0o600:
        raise RuntimeError('input device is not desktop-only mode 0600')
    Path('/tmp/native-input-device').write_text(device)
    deadline = time.monotonic() + 10
    while not Path('/run/native-input-denials-done').exists():
        if time.monotonic() >= deadline:
            raise RuntimeError('other-role input denial checks did not finish')
        time.sleep(.025)
    window = X.XCreateSimpleWindow(display, X.XDefaultRootWindow(display), 0, 0, 200, 100, 0, 0, 0)
    # A normal override-redirect test popup avoids Fluxbox's asynchronous
    # unmap/reparent cycle; input itself still comes from real uinput events.
    attributes = WindowAttributes(); attributes.override_redirect = 1
    X.XChangeWindowAttributes(display, window, 1 << 9, C.byref(attributes))
    X.XSelectInput(display, window, 3 | (1 << 17))
    X.XMapWindow(display, window)
    X.XSync(display, 0)
    # A running window manager must process MapRequest before focus is legal.
    mapped = False
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline and not mapped:
        select.select([X.XConnectionNumber(display)], [], [], .05)
        while X.XPending(display):
            event = Event(); X.XNextEvent(display, C.byref(event))
            if event.key.type == 19:  # MapNotify on our selected window
                mapped = True
    if not mapped:
        raise RuntimeError('window manager did not map input probe window')
    X.XSetInputFocus(display, window, 2, 0)
    X.XSync(display, 0)
    seen = set()
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline and seen != {2, 3}:
        for value in [1, 0]:
            os.write(keyboard, struct.pack('llHHi', 0, 0, 1, 30, value))
            os.write(keyboard, struct.pack('llHHi', 0, 0, 0, 0, 0))
        select.select([X.XConnectionNumber(display)], [], [], .2)
        while X.XPending(display):
            event = Event(); X.XNextEvent(display, C.byref(event))
            if event.key.type in [2, 3] and X.XLookupKeysym(C.byref(event.key), 0) == ord('a'):
                if event.key.send_event:
                    raise RuntimeError('received synthetic X event')
                seen.add(event.key.type)
    status = dict(line.split(':', 1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
    result = {'passed': seen == {2, 3}, 'uid': os.getuid(), 'device': device,
              'keyPress': 2 in seen, 'keyRelease': 3 in seen,
              'capEff': status['CapEff'].strip(), 'noNewPrivs': status['NoNewPrivs'].strip(),
              'seccomp': status['Seccomp'].strip(), 'syntheticXEvents': False}
    print(json.dumps(result), flush=True)
    if not result['passed'] or result['capEff'] != '0000000000000000' or result['noNewPrivs'] != '1':
        raise RuntimeError('physical guest input path failed')
finally:
    if created:
        fcntl.ioctl(keyboard, 0x5502)
    os.close(keyboard)
    X.XCloseDisplay(display)
