#!/usr/bin/env python3
"""Build the KVM Box whole-VM app image: the desktop's root, the same bytes on every platform it runs on.

python3 -I kvm-box/vm/build_image.py SPEC.json

SPEC.json (every field required unless marked optional; unknown fields refuse):
  out            new directory for the app image
  desktopRoot    an extracted desktop root tree (the native desktop's root image, unpacked); never modified
  mksquashfs     {"path": path, "sha256": hex}: the pinned packer the native desktop root is built with
  enclave        a checkout of the platform repository (the whole-VM front is built from isolation/vmboot/front)
  ownerPassword  a file holding the owner's password; created (0600, random) when absent; never printed
  models         optional: a model volume directory (volume.img, volume.json) as metal/volumes.mjs builds it

What comes out:
  root.img       the desktop root as squashfs: desktopRoot, /vm (start, the front, its ports), the helper built from
                 ../guest, and the owner's password hash. No kernel, no kernel module, nothing that names a platform.
  root.verity    its dm-verity tree
  APP.json       the root hash and geometry, the model volume's, the kernel modules the desktop needs loaded and its
                 protected ports

The platform wraps this for a host type (isolation/vmboot/build_boot.py in the platform repository: kernel, init,
modules, command line). The root hash is this image's identity on every platform; only the wrapper's launch identity
differs.

Nothing here talks to a host device or needs root: the tree is assembled in a private user and mount namespace as an
overlay over the read-only desktop root.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent
FIELDS = {'out', 'desktopRoot', 'mksquashfs', 'enclave', 'ownerPassword'}
OPTIONAL = {'models'}
KERNEL_MODULES = ['uhid', 'hid-generic', 'nf_tables']   # what the desktop needs from whatever kernel boots it
SALT_LABEL = b'enclave-kvm-box-vm-root/1\n'


def sha(path):
    with open(path, 'rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def run(argv, **kw):
    return subprocess.run([str(a) for a in argv], check=True, **kw)


def pinned(item, key='path'):
    p = Path(item[key]).resolve(strict=True)
    if not re.fullmatch('[0-9a-f]{64}', str(item.get('sha256'))) or sha(p) != item['sha256']:
        raise ValueError('pinned input differs: %s' % p)
    return p


def main(specpath):
    os.umask(0o022)
    specpath = Path(specpath).resolve()
    spec = json.loads(specpath.read_text())
    if not FIELDS <= set(spec) or set(spec) - FIELDS - OPTIONAL:
        raise ValueError('unsupported build specification')
    if os.getuid() != 0:
        # the password file is the owner's: make it before entering the namespace, where this user appears as root
        cred = Path(spec['ownerPassword'])
        if not cred.exists():
            fd = os.open(cred, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.write(fd, (secrets.token_urlsafe(32)+'\n').encode())
            os.close(fd)
        env = {**os.environ, 'KVMBOX_VM_BUILD_PARENT_USERNS': os.readlink('/proc/self/ns/user')}
        raise SystemExit(subprocess.call(['unshare', '--map-root-user', '--mount', '--fork', sys.executable, '-I',
                                          str(Path(__file__).resolve()), str(specpath)], env=env))
    if os.environ.get('KVMBOX_VM_BUILD_PARENT_USERNS') in (None, os.readlink('/proc/self/ns/user')):
        raise ValueError('requires a private unprivileged user namespace; host root refused')
    out = Path(spec['out']).resolve()
    base = Path(spec['desktopRoot']).resolve(strict=True)
    enclave = Path(spec['enclave']).resolve(strict=True)
    tool = pinned(spec['mksquashfs'])
    for name in ('xorg.conf', 'sunshine.conf', 'desktop-session.sh', 'usr/bin/busybox'):
        if not (base/name).is_file():
            raise ValueError('not a desktop root: missing '+name)
    if any(c in str(p) for p in (base, out) for c in ',:\\\n'):
        raise ValueError('unsafe overlay path syntax')
    password = Path(spec['ownerPassword']).read_text().strip()
    if len(password) < 20:
        raise ValueError('the owner password must be at least 20 characters')
    out.mkdir(mode=0o700)
    work = out/'work'
    for n in ('lower', 'upper', 'ovl', 'merged', 'bin'):
        (work/n).mkdir(parents=True)

    # what is compiled here: the front and the helper
    goenv = {**os.environ, 'CGO_ENABLED': '0', 'GOFLAGS': '-mod=readonly', 'HOME': os.environ.get('HOME', '/tmp')}
    gobuild = ['go', 'build', '-trimpath', '-buildvcs=false', '-ldflags=-s -w -buildid=', '-o']
    run([*gobuild, work/'bin/vmfront', './front'], cwd=enclave/'isolation/vmboot', env={**goenv, 'GOFLAGS': '-mod=mod', 'GOPROXY': 'off'})
    # the helper's go.mod names the platform checkout as a sibling directory; here it is wherever the spec says
    mod = (HERE.parent/'guest/go.mod').read_text().replace('../../../enclave/', str(enclave)+'/')
    (work/'helper.mod').write_text(mod)
    shutil.copyfile(enclave/'isolation/m2/go.sum', work/'helper.sum')
    run([*gobuild, work/'bin/helper', '-modfile', work/'helper.mod', '.'], cwd=HERE.parent/'guest',
        env={**goenv, 'GOFLAGS': '-mod=mod', 'GOPROXY': 'off'})

    # the root: an overlay over the desktop root, which stays read-only
    run(['mount', '--make-rprivate', '/'])
    lower, merged = work/'lower', work/'merged'
    run(['mount', '--bind', base, lower])
    run(['mount', '-o', 'remount,bind,ro', lower])
    run(['mount', '-t', 'overlay', 'overlay', '-o',
         'lowerdir=%s,upperdir=%s,workdir=%s,userxattr' % (lower, work/'upper', work/'ovl'), merged])
    try:
        (merged/'vm').mkdir()
        (merged/'models').mkdir(exist_ok=True)
        for name, mode in (('start', 0o555), ('app.ports', 0o444)):
            shutil.copyfile(HERE/name, merged/'vm'/name)
            os.chmod(merged/'vm'/name, mode)
        shutil.copyfile(work/'bin/vmfront', merged/'vm/vmfront')
        os.chmod(merged/'vm/vmfront', 0o555)
        for stale in ('helper', 'access.sha256'):
            (merged/stale).unlink(missing_ok=True)
        shutil.copyfile(work/'bin/helper', merged/'helper')
        os.chmod(merged/'helper', 0o555)
        (merged/'access.sha256').write_text(hashlib.sha256(password.encode()).hexdigest()+'\n')
        run([tool, merged, out/'root.img', '-noappend', '-comp', 'xz', '-b', '1M', '-processors', '8', '-mem', '2G',
             '-mkfs-time', '0', '-all-time', '0', '-all-root', '-no-xattrs', '-no-progress'], stdout=subprocess.DEVNULL)
    finally:
        run(['umount', merged])
        run(['umount', lower])
    size = (out/'root.img').stat().st_size
    if size % 4096:
        raise RuntimeError('the root image is not a whole number of blocks')
    blocks, salt = size // 4096, hashlib.sha256(SALT_LABEL).hexdigest()
    options = ['--no-superblock', '--format=1', '--hash=sha256', '--hash-offset=0', '--data-block-size=4096',
               '--hash-block-size=4096', '--data-blocks=%d' % blocks, '--salt='+salt]
    fmt = run(['veritysetup', 'format', *options, out/'root.img', out/'root.verity'], capture_output=True, text=True).stdout
    root_hash = re.search(r'^Root hash:\s*([0-9a-f]{64})$', fmt, re.M).group(1)
    run(['veritysetup', 'verify', *options, out/'root.img', out/'root.verity', root_hash], capture_output=True)
    os.chmod(out/'root.verity', 0o644)

    app = {'schema': 'enclave-vm-app/1', 'rootHash': root_hash, 'rootBlocks': blocks, 'rootSalt': salt,
           'artifacts': {n: {'bytes': (out/n).stat().st_size, 'sha256': sha(out/n)} for n in ('root.img', 'root.verity')},
           'kernelModules': KERNEL_MODULES, 'ports': (HERE/'app.ports').read_text().split(),
           'inputs': {'mksquashfsSha256': spec['mksquashfs']['sha256'], 'vmfrontSha256': sha(work/'bin/vmfront'),
                      'helperSha256': sha(work/'bin/helper'),
                      'sources': {n: sha(HERE/n) for n in ('start', 'app.ports', 'build_image.py')}}}
    if spec.get('models'):
        vol = Path(spec['models']).resolve(strict=True)
        v = json.loads((vol/'volume.json').read_text())['verity']
        if (v['alg'], v['dataBlockSize'], v['hashBlockSize']) != ('sha256', 4096, 4096) or v['hashStartBlock'] != v['dataBlocks']+1 \
                or not all(re.fullmatch('[0-9a-f]{64}', v[k]) for k in ('root', 'salt')):
            raise ValueError('the model volume is not in the layout the platform boot opens')
        if (vol/'volume.img').stat().st_size < (v['dataBlocks']+1)*4096:
            raise ValueError('the model volume is smaller than its geometry')
        app['models'] = {'dataBlocks': v['dataBlocks'], 'rootHash': v['root'], 'salt': v['salt']}
    (out/'APP.json').write_text(json.dumps(app, indent=1)+'\n')
    shutil.rmtree(work)
    print(json.dumps({'rootHash': root_hash, 'rootBytes': size}))


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('usage: build_image.py SPEC.json')
    main(sys.argv[1])
