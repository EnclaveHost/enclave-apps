#!/usr/bin/env python3
"""Prepare an owned copy of the recovered NucBox 6.6.87.2 tree for native desktop input.

This produces kernel build inputs only: nothing is built, and no hardware or
attestation qualification is performed. The source tree is a recovered local input
pinned by a caller-supplied manifest digest; it is not authenticated upstream source.
The checks here catch mistakes and drift. They are not a security boundary against a
malicious OS or a filesystem that is mutated concurrently.

Usage:
  prepare-kernel.py snapshot --source-tree SRC --output MANIFEST
      Write a source manifest and print its SHA-256. Review it and pin the digest
      separately; generating a manifest does not make it trusted.
  prepare-kernel.py --source-tree SRC --tree NEW --source-manifest MANIFEST
      --source-manifest-sha256 HEX --driver DRIVER --receipt NEW_RECEIPT
"""
import argparse
import hashlib
import json
import os
import re
import secrets
import shutil
import stat
import subprocess
import sys
from pathlib import Path

BASE_CONFIG = '9cbd23301bd7563331c046298b9c22effb2cc9c5035f51aceef27590497a3209'
TPM_DRIVER = 'c9bde9f889869a79543f69e21eaddcc78290eee9a16514e91252b73a4b131c56'
MANIFEST_SCHEMA = 'kvm-box-kernel-source/1'
RECEIPT_SCHEMA = 'kvm-box-hyperv-kernel-preparation/2'
MAKEFILE_VERSION = (('VERSION', '6'), ('PATCHLEVEL', '6'), ('SUBLEVEL', '87'), ('EXTRAVERSION', '.2'))
BASE_RELEASE = '6.6.87.2-microsoft-standard-WSL2'
LOCALVERSION = '-enclave-kvm-box'
MAKE = '/usr/bin/make'
BASH = '/usr/bin/bash'
ENV = {'PATH': '/usr/bin:/bin', 'LC_ALL': 'C', 'ARCH': 'x86_64', 'KCONFIG_CONFIG': '.config'}
ENABLED = ('CONFIGFS_FS', 'INPUT_EVDEV', 'INPUT_UINPUT', 'HID', 'HID_GENERIC', 'UHID', 'NFT_REJECT',
           'NFT_REJECT_INET', 'TCG_ENCLAVE_OPENHCL')
REQUIRED = ('HYPERV', 'HYPERV_VSOCKETS', 'TCG_TPM', 'EXT4_FS', 'BLK_DEV_DM', 'DM_VERITY', 'NF_TABLES',
            'NF_TABLES_INET', 'SECCOMP_FILTER', 'DEVTMPFS', 'TMPFS', 'RD_XZ')
# The complete set of .config changes preparation may make: (base value, prepared value).
# 'n' means "# CONFIG_X is not set"; None means the symbol is absent.
EXPECTED_DIFF = {
    **{key: ('m', 'y') for key in (
        'CONFIGFS_FS', 'HID', 'HID_GENERIC', 'INPUT_EVDEV', 'INPUT_UINPUT', 'NFT_REJECT', 'NFT_REJECT_INET',
        'NFT_REJECT_IPV4', 'NFT_REJECT_IPV6', 'NF_REJECT_IPV4', 'NF_REJECT_IPV6')},
    'UHID': ('n', 'y'),
    'TCG_ENCLAVE_OPENHCL': (None, 'y'),
    'LOCALVERSION': ('"-microsoft-standard-WSL2"', f'"{LOCALVERSION}"'),
    'PAHOLE_VERSION': ('131', '132'),
}
TPM_DIR = 'drivers/char/tpm'
DRIVER_PATH = f'{TPM_DIR}/enclave_openhcl_tpm.c'
REQUIRED_FILES = ('Makefile', '.config', 'include/config/kernel.release', 'scripts/config',
                  f'{TPM_DIR}/Kconfig', f'{TPM_DIR}/Makefile')
KCONFIG_ENTRY = b'''
config TCG_ENCLAVE_OPENHCL
\tbool "Enclave OpenHCL guest TPM transport"
\tdepends on TCG_TPM && X86_64 && HYPERV
\thelp
\t  Guest-only OpenHCL CRB transport for the measured Linux direct-boot image.
'''
MAKEFILE_ENTRY = b'\nobj-$(CONFIG_TCG_ENCLAVE_OPENHCL) += enclave_openhcl_tpm.o\n'
ENTRY_FIELDS = {
    'directory': {'path', 'kind', 'mode'},
    'file': {'path', 'kind', 'mode', 'bytes', 'sha256'},
    'symlink': {'path', 'kind', 'mode', 'target'},
}
HEX64 = re.compile(r'[0-9a-f]{64}')


class PreparationError(Exception):
    """A source, path, pin or configuration validation failed."""


class PreparationFailed(Exception):
    """Failed after the journal was created; the tree is retained and the journal says why."""


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def canonical(document):
    return (json.dumps(document, sort_keys=True, separators=(',', ':')) + '\n').encode()


def is_within(path, root):
    path, root = Path(path), Path(root)
    return path == root or root in path.parents


def read_nofollow(path):
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as f:
        return f.read()


def snapshot(root):
    """Return sorted manifest entries for everything under root (root itself excluded).

    Uses lstat throughout and refuses anything a plain copy would not reproduce:
    special files, hard-linked regular files and symlinks that leave the tree.
    """
    root = os.fspath(root)
    real_root = os.path.realpath(root)
    entries = []

    def walk(directory, prefix):
        with os.scandir(directory) as it:
            names = sorted(entry.name for entry in it)
        for name in names:
            rel, path = prefix + name, os.path.join(directory, name)
            st = os.lstat(path)
            mode = stat.S_IMODE(st.st_mode)
            if stat.S_ISDIR(st.st_mode):
                entries.append({'path': rel, 'kind': 'directory', 'mode': mode})
                walk(path, rel + '/')
            elif stat.S_ISREG(st.st_mode):
                if st.st_nlink != 1:
                    raise PreparationError(f'hard-linked file in tree: {rel}')
                data = read_nofollow(path)
                entries.append({'path': rel, 'kind': 'file', 'mode': mode, 'bytes': len(data), 'sha256': sha256(data)})
            elif stat.S_ISLNK(st.st_mode):
                target = os.readlink(path)
                lexical = os.path.normpath(os.path.join(os.path.dirname(rel), target))
                if (os.path.isabs(target) or lexical == '..' or lexical.startswith('../')
                        or not is_within(os.path.realpath(path), real_root)):
                    raise PreparationError(f'symlink leaves the tree: {rel} -> {target}')
                entries.append({'path': rel, 'kind': 'symlink', 'mode': mode, 'target': target})
            else:
                raise PreparationError(f'special file in tree: {rel}')

    walk(root, '')
    entries.sort(key=lambda entry: entry['path'])
    return entries


def manifest_document(root):
    return {'schema': MANIFEST_SCHEMA, 'entries': snapshot(root)}


def snapshot_digest(root):
    return sha256(canonical(manifest_document(root)))


def _reject_duplicate_keys(pairs):
    keys = [key for key, _ in pairs]
    if len(keys) != len(set(keys)):
        raise PreparationError('duplicate JSON key in source manifest')
    return dict(pairs)


def valid_relative(path):
    return (isinstance(path, str) and path != '' and not path.startswith('/') and '\0' not in path
            and all(part not in ('', '.', '..') for part in path.split('/')))


def parse_manifest(data):
    try:
        document = json.loads(data.decode('utf-8'), object_pairs_hook=_reject_duplicate_keys)
    except (UnicodeDecodeError, ValueError) as error:
        raise PreparationError(f'source manifest is not valid JSON: {error}') from None
    if (not isinstance(document, dict) or set(document) != {'schema', 'entries'}
            or document['schema'] != MANIFEST_SCHEMA or not isinstance(document['entries'], list)):
        raise PreparationError(f'source manifest does not use schema {MANIFEST_SCHEMA}')
    previous = None
    for entry in document['entries']:
        kind = entry.get('kind') if isinstance(entry, dict) else None
        if not isinstance(kind, str) or set(entry) != ENTRY_FIELDS.get(kind):
            raise PreparationError(f'malformed source manifest entry: {entry!r}')
        path = entry['path']
        if not valid_relative(path):
            raise PreparationError(f'invalid source manifest path: {path!r}')
        if previous is not None and path <= previous:
            raise PreparationError(f'source manifest entries are unsorted or duplicated at {path!r}')
        previous = path
        if type(entry['mode']) is not int or not 0 <= entry['mode'] <= 0o7777:
            raise PreparationError(f'invalid mode in source manifest: {path!r}')
        if kind == 'file' and (type(entry['bytes']) is not int or entry['bytes'] < 0
                               or not isinstance(entry['sha256'], str) or not HEX64.fullmatch(entry['sha256'])):
            raise PreparationError(f'invalid file entry in source manifest: {path!r}')
        if kind == 'symlink' and (not isinstance(entry['target'], str) or entry['target'] == ''):
            raise PreparationError(f'invalid symlink entry in source manifest: {path!r}')
    return document['entries']


def require_same(expected, actual, what):
    if expected == actual:
        return
    want = {entry['path']: entry for entry in expected}
    have = {entry['path']: entry for entry in actual}
    missing = sorted(want.keys() - have.keys())
    extra = sorted(have.keys() - want.keys())
    changed = sorted(path for path in want.keys() & have.keys() if want[path] != have[path])
    raise PreparationError(f'{what} differs from the pinned manifest: '
                           f'missing={missing[:5]} extra={extra[:5]} changed={changed[:5]}')


def plain_path(raw, what):
    """Absolute path whose parent is an existing directory reached without symlinks."""
    if '..' in Path(raw).parts:
        raise PreparationError(f'{what} must not contain "..": {raw}')
    path = Path(os.path.abspath(raw))
    if not path.parent.is_dir() or os.path.realpath(path.parent) != str(path.parent):
        raise PreparationError(f'{what} parent must be an existing directory reached without symlinks: {path.parent}')
    return path


def source_dir(raw):
    source = plain_path(raw, '--source-tree')
    if os.path.islink(source) or not source.is_dir():
        raise PreparationError(f'--source-tree must be an existing directory, not a symlink: {source}')
    return source


def new_path(raw, what):
    path = plain_path(raw, what)
    if os.path.lexists(path):
        raise PreparationError(f'{what} already exists; refusing to reuse or clobber it: {path}')
    return path


def check_makefile_version(data):
    text = data.decode('utf-8', 'replace')
    for key, want in MAKEFILE_VERSION:
        match = re.search(rf'^{key}[ \t]*=[ \t]*(\S*)[ \t]*$', text, re.M)
        if not match or match.group(1) != want:
            raise PreparationError(f'source Makefile {key} is not {want!r}; expected kernel 6.6.87.2')


def tool_record(path):
    data = read_nofollow(path)
    try:
        result = subprocess.run([path, '--version'], cwd='/', env=dict(ENV), check=True, capture_output=True, text=True)
    except (OSError, subprocess.CalledProcessError) as error:
        raise PreparationError(f'cannot query tool version for {path}: {error}') from None
    return {'path': path, 'sha256': sha256(data), 'version': (result.stdout.splitlines() or [''])[0]}


def commands():
    config = [BASH, 'scripts/config', '--file', '.config', '--set-str', 'LOCALVERSION', LOCALVERSION,
              '--disable', 'LOCALVERSION_AUTO']
    for key in ENABLED:
        config += ['--enable', key]
    return [config, [MAKE, 'olddefconfig']]


def owned_target(tree, rel, create=False):
    """Path inside tree that is safe to write: no symlinked parents, and a single-link regular file."""
    path = tree / rel
    if os.path.realpath(path.parent) != str(path.parent):
        raise PreparationError(f'refusing to write through a symlinked directory: {rel}')
    if not create:
        st = os.lstat(path)
        if not stat.S_ISREG(st.st_mode) or st.st_nlink != 1:
            raise PreparationError(f'refusing to edit a symlink, hard link or special file: {rel}')
    return path


def read_owned(tree, rel):
    return read_nofollow(owned_target(tree, rel))


def write_owned(tree, rel, data, create=False):
    path = owned_target(tree, rel, create)
    flags = os.O_WRONLY | os.O_NOFOLLOW | (os.O_CREAT | os.O_EXCL if create else os.O_TRUNC)
    with os.fdopen(os.open(path, flags, 0o644), 'wb') as f:
        f.write(data)


def parse_config(data):
    config = {}
    for line in data.decode('utf-8').splitlines():
        match = re.fullmatch(r'CONFIG_(\w+)=(.*)', line)
        unset = re.fullmatch(r'# CONFIG_(\w+) is not set', line)
        if not (match or unset):
            continue
        key, value = (match.group(1), match.group(2)) if match else (unset.group(1), 'n')
        if key in config:
            raise PreparationError(f'duplicate config symbol: {key}')
        config[key] = value
    return config


def check_config(before, after):
    diff = {key: (before.get(key), after.get(key))
            for key in sorted(before.keys() | after.keys()) if before.get(key) != after.get(key)}
    if diff != EXPECTED_DIFF:
        unexpected = {key: value for key, value in diff.items() if EXPECTED_DIFF.get(key) != value}
        missing = sorted(key for key, value in EXPECTED_DIFF.items() if diff.get(key) != value)
        raise PreparationError(f'.config diff is not the expected one: unexpected={unexpected} missing={missing}')
    for key in (*ENABLED, *REQUIRED):
        if after.get(key) != 'y':
            raise PreparationError(f'required built-in feature is not enabled: {key}')
    if after.get('LOCALVERSION_AUTO') != 'n' or after.get('LOCALVERSION') != f'"{LOCALVERSION}"':
        raise PreparationError('LOCALVERSION settings were not applied')
    return {key: list(value) for key, value in diff.items()}


def patch(tree, driver):
    base_config = read_owned(tree, '.config')
    if sha256(base_config) != BASE_CONFIG:
        raise PreparationError('cloned .config differs from the pinned base configuration')
    kconfig = read_owned(tree, f'{TPM_DIR}/Kconfig') + KCONFIG_ENTRY
    makefile = read_owned(tree, f'{TPM_DIR}/Makefile') + MAKEFILE_ENTRY
    write_owned(tree, DRIVER_PATH, driver, create=True)
    write_owned(tree, f'{TPM_DIR}/Kconfig', kconfig)
    write_owned(tree, f'{TPM_DIR}/Makefile', makefile)
    for command in commands():
        subprocess.run(command, cwd=tree, env=dict(ENV), check=True, capture_output=True, text=True)
    final_config = read_owned(tree, '.config')
    diff = check_config(parse_config(base_config), parse_config(final_config))
    return {
        'configDiff': diff,
        'preparedInputs': {'.config': sha256(final_config), f'{TPM_DIR}/Kconfig': sha256(kconfig),
                           f'{TPM_DIR}/Makefile': sha256(makefile), DRIVER_PATH: sha256(driver)},
        'preparedSnapshotSha256': snapshot_digest(tree),
    }


def write_journal(receipt, record, create=False):
    data = (json.dumps(record, indent=2, sort_keys=True) + '\n').encode()
    target = receipt if create else receipt.with_name(f'.{receipt.name}.{secrets.token_hex(8)}.tmp')
    with open(target, 'xb') as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    if not create:
        os.replace(target, receipt)


def prepare(args):
    helper_sha256 = sha256(Path(__file__).read_bytes())
    source = source_dir(args.source_tree)
    tree = new_path(args.tree, '--tree')
    receipt = new_path(args.receipt, '--receipt')
    if is_within(tree, source):
        raise PreparationError('--tree must be outside --source-tree')
    if is_within(receipt, source) or is_within(receipt, tree):
        raise PreparationError('--receipt must be outside --source-tree and --tree')
    if not HEX64.fullmatch(args.source_manifest_sha256):
        raise PreparationError('--source-manifest-sha256 must be 64 lowercase hex characters')

    manifest = Path(args.source_manifest).read_bytes()
    if sha256(manifest) != args.source_manifest_sha256:
        raise PreparationError('source manifest digest does not match --source-manifest-sha256')
    entries = parse_manifest(manifest)
    source_sha256 = sha256(canonical({'schema': MANIFEST_SCHEMA, 'entries': entries}))
    require_same(entries, snapshot(source), 'source tree')
    by_path = {entry['path']: entry for entry in entries}
    for rel in REQUIRED_FILES:
        if by_path.get(rel, {}).get('kind') != 'file':
            raise PreparationError(f'source tree lacks required regular file: {rel}')
    if DRIVER_PATH in by_path:
        raise PreparationError('source tree is already patched')
    if by_path['.config']['sha256'] != BASE_CONFIG:
        raise PreparationError('source .config differs from the pinned NucBox base configuration')
    check_makefile_version(read_nofollow(source / 'Makefile'))
    if read_nofollow(source / 'include/config/kernel.release').decode('utf-8', 'replace').strip() != BASE_RELEASE:
        raise PreparationError('generated kernel.release does not match the base release')
    driver = Path(args.driver).read_bytes()
    if sha256(driver) != TPM_DRIVER:
        raise PreparationError('OpenHCL guest TPM transport differs from the reviewed input')

    record = {
        'schema': RECEIPT_SCHEMA,
        'status': 'preparing',
        'helperSha256': helper_sha256,
        'sourceTree': str(source),
        'tree': str(tree),
        'sourceManifestSha256': args.source_manifest_sha256,
        'sourceSnapshotSha256': source_sha256,
        'baseConfigSha256': BASE_CONFIG,
        'driverSha256': TPM_DRIVER,
        'kernelVersion': '6.6.87.2',
        'tools': {'make': tool_record(MAKE), 'bash': tool_record(BASH)},
        'environment': dict(ENV),
        'commands': commands(),
        'enabledBuiltins': list(ENABLED),
        'requiredBuiltins': list(REQUIRED),
        'localVersion': LOCALVERSION,
        'scope': 'kernel source preparation only; no build, hardware qualification or attestation',
        'sourceProvenance': 'recovered local input pinned by a caller-supplied manifest digest; '
                            'not authenticated upstream source',
    }
    write_journal(receipt, record, create=True)
    try:
        tree.mkdir(exist_ok=False)
        shutil.copytree(source, tree, dirs_exist_ok=True, symlinks=True)
        require_same(entries, snapshot(tree), 'cloned tree')
        record.update(patch(tree, driver))
        record['status'] = 'success'
    except Exception as error:
        record.update(status='failed', error=f'{type(error).__name__}: {error}', retainedTree=str(tree))
        write_journal(receipt, record)
        raise PreparationFailed(f'{error}; partial tree retained at {tree}; failure journal at {receipt}') from error
    write_journal(receipt, record)
    return tree, receipt


def snapshot_main(argv):
    p = argparse.ArgumentParser(prog='prepare-kernel.py snapshot',
                                description='Write a source manifest; pin its printed digest separately.')
    p.add_argument('--source-tree', required=True)
    p.add_argument('--output', required=True)
    a = p.parse_args(argv)
    try:
        source = source_dir(a.source_tree)
        output = new_path(a.output, '--output')
        if is_within(output, source):
            raise PreparationError('--output must be outside --source-tree')
        data = canonical(manifest_document(source))
        with open(output, 'xb') as f:
            f.write(data)
    except (PreparationError, OSError) as error:
        print(f'refused: {error}', file=sys.stderr)
        return 2
    print(sha256(data))
    return 0


def main(argv=None):
    argv = sys.argv[1:] if argv is None else list(argv)
    if argv[:1] == ['snapshot']:
        return snapshot_main(argv[1:])
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for flag in ('--source-tree', '--tree', '--source-manifest', '--source-manifest-sha256', '--driver', '--receipt'):
        p.add_argument(flag, required=True)
    a = p.parse_args(argv)
    try:
        tree, receipt = prepare(a)
    except (PreparationError, OSError) as error:
        print(f'refused: {error}', file=sys.stderr)
        return 2
    except PreparationFailed as error:
        print(f'preparation failed: {error}', file=sys.stderr)
        return 1
    print(f'Prepared kernel source copy at {tree} (receipt {receipt}). Preparation only: '
          'nothing was built, and no hardware or attestation qualification was performed.')
    return 0


if __name__ == '__main__':
    sys.exit(main())
