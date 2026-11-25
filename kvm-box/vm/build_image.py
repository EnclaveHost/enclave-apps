#!/usr/bin/env python3
"""Build the KVM Box whole-VM image: one SEV-SNP guest for the platform's whole-VM path (catalog runtime "sev-snp-vm").

python3 -I kvm-box/vm/build_image.py SPEC.json

SPEC.json (every field required unless marked optional; unknown fields refuse):
  out            new directory for the image
  desktopRoot    an extracted desktop root tree (the native desktop's root image, unpacked); never modified
  kernel         {"vmlinuz": path, "sha256": hex}: the guest kernel
  kernelRoot     {"image": path, "sha256": hex, "release": "6.8.0-147-generic"}: the ext4 image that carries that
                 kernel's modules under /usr/lib/modules/<release> (the RPC guest image's root.img)
  mksquashfs     {"path": path, "sha256": hex}: the pinned packer the native desktop root is built with
  enclave        a checkout of the platform repository (the whole-VM front is built from isolation/m2/vmfront)
  ownerPassword  a file holding the owner's password; created (0600, random) when absent; never printed
  models         optional: a model volume directory (volume.img, volume.json) as metal/volumes.mjs builds it

What comes out, in the layout the platform's launcher (rpc/snp/launch.py) boots:
  vmlinuz        the kernel, unchanged
  initrd.img     vm-init (static) and the three kernel modules it needs before the root is verified
  root.img       the desktop root as squashfs: desktopRoot, the remaining kernel modules, /vm (start, the front, its
                 ports), the helper built from ../guest, and the owner's password hash
  root.verity    its dm-verity tree
  IMAGE.json     sha256 of each, the root hash and the kernel command line SEV-SNP measures with kernel and initrd

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
FIELDS = {'out', 'desktopRoot', 'kernel', 'kernelRoot', 'mksquashfs', 'enclave', 'ownerPassword'}
OPTIONAL = {'models'}
EARLY = ['dm-verity', 'overlay']                      # needed to verify and mount the root: in the initramfs
LATE = ['tsm', 'sev-guest', 'uhid', 'hid-generic', 'nf_tables']   # loaded from the verified root
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


def module_order(modules, names):
    """The files to load for these modules, dependencies first, from the kernel's own modules.dep."""
    dep = {}
    for line in (modules/'modules.dep').read_text().splitlines():
        a, b = line.split(':', 1)
        dep[a] = b.split()
    builtin = (modules/'modules.builtin').read_text()
    out = []

    def add(f):
        for d in reversed(dep[f]):
            add(d)
        if f not in out:
            out.append(f)
    for m in names:
        spellings = {m, m.replace('-', '_'), m.replace('_', '-')}
        found = [f for f in dep if any(f.endswith('/%s.ko.zst' % s) or f.endswith('/%s.ko' % s) for s in spellings)]
        if len(found) == 1:
            add(found[0])
        elif found or not any('/%s.ko' % s in builtin for s in spellings):
            raise ValueError('kernel module %s is neither built in nor a single module' % m)
    return out


def place_modules(modules, files, dest, prefix):
    """Decompressed copies under dest; returns the load-order list as the guest will see the paths."""
    dest.mkdir(parents=True, exist_ok=True)
    lines = []
    for f in files:
        name = Path(f).name.removesuffix('.zst')
        if f.endswith('.zst'):
            run(['zstd', '-dqf', modules/f, '-o', dest/name])
        else:
            shutil.copyfile(modules/f, dest/name)
        os.chmod(dest/name, 0o444)
        lines.append('%s/%s' % (prefix, name))
    return '\n'.join(lines)+'\n'


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
    vmlinuz = pinned(spec['kernel'], 'vmlinuz')
    kroot = pinned(spec['kernelRoot'], 'image')
    release = spec['kernelRoot']['release']
    if not re.fullmatch(r'[0-9A-Za-z.+-]{1,64}', release):
        raise ValueError('kernel release')
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
    for n in ('lower', 'upper', 'ovl', 'merged', 'initrd', 'kernel', 'bin'):
        (work/n).mkdir(parents=True)

    # the kernel's modules, from the image that carries them
    run(['/usr/sbin/debugfs', '-R', 'rdump /usr/lib/modules/%s %s' % (release, work/'kernel'), kroot],
        capture_output=True)
    modules = work/'kernel'/release
    if not (modules/'modules.dep').is_file():
        raise ValueError('the kernel root has no modules for '+release)

    # what is compiled here: the measured init, the front, the helper
    run(['gcc', '-static', '-O2', '-Wall', '-Wextra', '-Werror', HERE/'vm-init.c', '-o', work/'initrd/init'])
    goenv = {**os.environ, 'CGO_ENABLED': '0', 'GOFLAGS': '-mod=readonly', 'HOME': os.environ.get('HOME', '/tmp')}
    gobuild = ['go', 'build', '-trimpath', '-buildvcs=false', '-ldflags=-s -w -buildid=', '-o']
    run([*gobuild, work/'bin/vmfront', './vmfront'], cwd=enclave/'isolation/m2', env=goenv)
    # the helper's go.mod names the platform checkout as a sibling directory; here it is wherever the spec says
    mod = (HERE.parent/'guest/go.mod').read_text().replace('../../../enclave/', str(enclave)+'/')
    (work/'helper.mod').write_text(mod)
    shutil.copyfile(enclave/'isolation/m2/go.sum', work/'helper.sum')
    run([*gobuild, work/'bin/helper', '-modfile', work/'helper.mod', '.'], cwd=HERE.parent/'guest',
        env={**goenv, 'GOFLAGS': '-mod=mod', 'GOPROXY': 'off'})

    # the initramfs: init, its modules, and the mount points it uses
    initrd = work/'initrd'
    (initrd/'module-order.txt').write_text(place_modules(modules, module_order(modules, EARLY), initrd/'modules', '/modules'))
    for d in ('proc', 'sys', 'dev', 'lower', 'rw', 'newroot'):
        (initrd/d).mkdir()
    for p in [initrd, *initrd.rglob('*')]:
        os.utime(p, (0, 0), follow_symlinks=False)
    entries = sorted(str(p.relative_to(initrd)) for p in initrd.rglob('*'))
    with open(out/'initrd.img', 'wb') as f:
        cpio = subprocess.Popen(['cpio', '-o', '-H', 'newc', '--reproducible', '--owner=0:0', '--quiet'], cwd=initrd,
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        gz = subprocess.Popen(['gzip', '-n', '-6'], stdin=cpio.stdout, stdout=f)
        cpio.stdout.close()
        cpio.communicate(('\n'.join(entries)+'\n').encode())
        if cpio.returncode or gz.wait():
            raise RuntimeError('packing the initramfs failed')

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
        (merged/'vm/module-order.txt').write_text(
            place_modules(modules, [f for f in module_order(modules, LATE) if f not in module_order(modules, EARLY)],
                          merged/'usr/lib/modules/vm', '/usr/lib/modules/vm'))
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

    cmdline = 'console=ttyS0 panic=-1 enclave.roothash=%s enclave.root=%d:%s' % (root_hash, blocks, salt)
    ro_disks = []
    if spec.get('models'):
        vol = Path(spec['models']).resolve(strict=True)
        v = json.loads((vol/'volume.json').read_text())['verity']
        if (v['alg'], v['dataBlockSize'], v['hashBlockSize']) != ('sha256', 4096, 4096) or v['hashStartBlock'] != v['dataBlocks']+1 \
                or not all(re.fullmatch('[0-9a-f]{64}', v[k]) for k in ('root', 'salt')):
            raise ValueError('the model volume is not in the layout vm-init opens')
        if (vol/'volume.img').stat().st_size < (v['dataBlocks']+1)*4096:
            raise ValueError('the model volume is smaller than its geometry')
        cmdline += ' enclave.models=%d:%s:%s' % (v['dataBlocks'], v['root'], v['salt'])
        ro_disks.append({'serial': 'MODELS', 'file': str(vol/'volume.img'), 'verityRoot': v['root']})
    shutil.copyfile(vmlinuz, out/'vmlinuz')
    image = {'schema': 'enclave-kvm-box-vm-image/1', 'kernel': release, 'cmdline': cmdline, 'rootHash': root_hash,
             'artifacts': {n: {'bytes': (out/n).stat().st_size, 'sha256': sha(out/n)}
                           for n in ('vmlinuz', 'initrd.img', 'root.img', 'root.verity')},
             'roDisks': ro_disks, 'ports': (HERE/'app.ports').read_text().split(),
             'inputs': {'kernelRootSha256': spec['kernelRoot']['sha256'], 'mksquashfsSha256': spec['mksquashfs']['sha256'],
                        'vmfrontSha256': sha(work/'bin/vmfront'), 'helperSha256': sha(work/'bin/helper'),
                        'sources': {n: sha(HERE/n) for n in ('vm-init.c', 'start', 'app.ports', 'build_image.py')}}}
    (out/'IMAGE.json').write_text(json.dumps(image, indent=1)+'\n')
    shutil.rmtree(work)
    print(json.dumps({'rootHash': root_hash, 'cmdline': cmdline, 'rootBytes': size}))


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('usage: build_image.py SPEC.json')
    main(sys.argv[1])
