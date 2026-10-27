#!/usr/bin/env python3
"""Stage measured static UI files; never import writable desktop home content.

The output is a fresh tree for /usr/share/kvm-box/native-ui in the new image.
Image packaging must assign UID/GID 0 and mount it read-only. This build tool
records byte identity, not image admission or content authenticity.
"""
import argparse
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import stat

MAX_FILE = 16 << 20
MAX_TOTAL = 64 << 20
MAX_FILES = 512


def read_regular(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as stream:
        st = os.fstat(stream.fileno())
        if not stat.S_ISREG(st.st_mode) or st.st_size > MAX_FILE:
            raise ValueError('invalid static asset type or size')
        data = stream.read(MAX_FILE + 1)
    if len(data) > MAX_FILE:
        raise ValueError('static asset grew beyond limit')
    return data


def collect(novnc, native_page):
    root = Path(novnc).resolve(strict=True)
    if not root.is_dir():
        raise ValueError('noVNC input must be a directory')
    output = {}
    active_dirs = set()
    total = 0

    rootfd = os.open(root, os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
    class OpenHow(ctypes.Structure):
        _fields_ = [('flags',ctypes.c_uint64),('mode',ctypes.c_uint64),('resolve',ctypes.c_uint64)]
    libc = ctypes.CDLL(None,use_errno=True)
    libc.syscall.restype = ctypes.c_long
    visited = 0

    def visit(source, relative):
        nonlocal total, visited
        visited += 1
        if visited > 2048 or len(relative.parts) > 16 or len(str(relative)) > 1024:
            raise ValueError('static asset tree/path limit')
        # Kernel confinement is enforced on every lookup, including parents.
        # Relative aliases may stay beneath the pinned root; escapes and magic
        # links are refused even if a source directory changes during the build.
        how = OpenHow(os.O_PATH | os.O_CLOEXEC,0,0x08 | 0x02)
        fd = libc.syscall(437,rootfd,ctypes.c_char_p(str(source).encode()),ctypes.byref(how),ctypes.sizeof(how))
        if fd < 0:
            e = ctypes.get_errno()
            if e in (errno.EXDEV,errno.ELOOP):
                raise ValueError('static alias escapes root or loops')
            raise OSError(e,os.strerror(e))
        try:
            st = os.fstat(fd)
            if not (stat.S_ISDIR(st.st_mode) or stat.S_ISREG(st.st_mode)):
                raise ValueError('nonregular static asset')
            # Reopen our pinned descriptor, not a source pathname that could
            # have been replaced after validation. Never open special devices.
            readfd = os.open(f'/proc/self/fd/{fd}',os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
            if stat.S_ISDIR(st.st_mode):
                identity = (st.st_dev, st.st_ino)
                if identity in active_dirs:
                    os.close(readfd)
                    raise ValueError('static directory alias cycle')
                active_dirs.add(identity)
                try:
                    with os.scandir(readfd) as entries:
                        for child in entries:
                            visit(source/child.name,relative/child.name)
                finally:
                    active_dirs.remove(identity)
                    os.close(readfd)
            else:
                with os.fdopen(readfd,'rb') as stream:
                    if st.st_size > MAX_FILE:
                        raise ValueError('static asset file limit')
                    data = stream.read(MAX_FILE+1)
                total += len(data)
                if len(data) > MAX_FILE or len(output) >= MAX_FILES or total > MAX_TOTAL:
                    raise ValueError('static asset tree limit')
                output[str(relative)] = data
        finally:
            os.close(fd)

    try:
        for name in ['vnc.html', 'app', 'core', 'vendor']:
            visit(Path(name), Path('novnc')/name)
    finally:
        os.close(rootfd)
    native_page = Path(native_page)
    if native_page.is_symlink():
        raise ValueError('native UI page must be a regular source file')
    data = read_regular(native_page)
    if len(data) > 512 << 10 or total + len(data) > MAX_TOTAL or len(output) >= MAX_FILES:
        raise ValueError('native UI page limit')
    output['native.html'] = data
    # Package the bounded x11vnc polling adapter with the measured UI. Never
    # load code from mutable home files or a separate, unverified web origin.
    pacing = read_regular(Path(__file__).with_name('desktop-pacing.mjs'))
    bootstrap = b'''import UI from './ui.js';
import RFB from '../core/rfb.js';
import {installPacing} from './native-pacing.mjs';
let current=null, pacing=null;
setInterval(()=>{
  if(UI.rfb!==current){pacing?.stop();current=UI.rfb;pacing=null;}
  if(current?._rfbConnectionState==='connected' && !pacing && !current._enabledContinuousUpdates){
    pacing=installPacing(current,RFB.messages.fbUpdateRequest);
  }
},500);
'''
    html = output['novnc/vnc.html']
    if html.count(b'</body>') != 1:
        raise ValueError('noVNC page must have one closing body')
    tag = b'<script type="module" src="app/native-pacing-bootstrap.mjs"></script>\n'
    if len(html)+len(tag)>MAX_FILE or len(pacing)>MAX_FILE or total+len(data)+len(pacing)+len(bootstrap)+len(tag)>MAX_TOTAL or len(output)+2>MAX_FILES:
        raise ValueError('desktop pacing asset limit')
    output['novnc/vnc.html'] = html.replace(b'</body>',tag+b'</body>')
    output['novnc/app/native-pacing.mjs'] = pacing
    output['novnc/app/native-pacing-bootstrap.mjs'] = bootstrap
    return output


def build(novnc, native_page, destination):
    destination = Path(destination)
    root = Path(novnc).resolve(strict=True)
    if destination.resolve().is_relative_to(root):
        raise ValueError('output must be outside source tree')
    if destination.exists() or destination.is_symlink():
        raise FileExistsError('output must be a fresh directory')
    contents = collect(novnc, native_page)
    # Validate all source contents before creating the output. A failed output
    # write leaves only a bounded new staging tree, never resets an old image.
    destination.mkdir(parents=False, mode=0o755)
    entries = []
    for relative, data in sorted(contents.items()):
        target = destination/relative
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
        with target.open('xb') as f:
            f.write(data)
        target.chmod(0o444)
        entries.append({'path':relative, 'bytes':len(data),
                        'sha256':hashlib.sha256(data).hexdigest(), 'mode':0o444})
    manifest = {'schema':'kvm-box-native-ui-assets/1', 'files':entries,
                'imageOwnerUid':0, 'imageOwnerGid':0, 'readOnlyImageRequired':True,
                'admissionEstablished':False}
    p = destination/'assets-manifest.json'
    p.write_text(json.dumps(manifest,indent=2)+'\n')
    p.chmod(0o444)
    for directory in sorted((p for p in destination.rglob('*') if p.is_dir()), key=lambda p:len(p.parts), reverse=True):
        directory.chmod(0o555)
    destination.chmod(0o555)
    return manifest


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--novnc', required=True, type=Path)
    parser.add_argument('--native-page', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args=parser.parse_args()
    manifest=build(args.novnc,args.native_page,args.output)
    print(json.dumps({'files':len(manifest['files']), 'bytes':sum(x['bytes'] for x in manifest['files'])}))


if __name__=='__main__':
    main()
