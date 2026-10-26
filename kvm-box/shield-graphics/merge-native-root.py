#!/usr/bin/env python3
"""Apply graphics-image changes to a customized VM base without losing its assets.

Inputs are immutable development exports, not live guest filesystems. All
changes and conflicts are checked before a new output directory is created.
This composes a build input; it does not mount, launch or publish a VM.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat

EPHEMERAL = {'dev', 'proc', 'sys', 'run', 'tmp'}

def inventory(root):
    result = {}
    def error(exc): raise exc
    for directory, dirs, files in os.walk(root, followlinks=False, onerror=error):
        if Path(directory) == root:
            dirs[:] = [name for name in dirs if name not in EPHEMERAL]
        for name in dirs + files:
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            if relative.split('/')[0] in EPHEMERAL:
                continue
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                entry = {'kind': 'link', 'target': os.readlink(path)}
            elif stat.S_ISDIR(mode):
                entry = {'kind': 'directory', 'mode': stat.S_IMODE(mode)}
            elif stat.S_ISREG(mode):
                with path.open('rb') as stream:
                    digest = hashlib.file_digest(stream, 'sha256').hexdigest()
                entry = {'kind': 'file', 'mode': stat.S_IMODE(mode), 'sha256': digest}
            else:
                raise ValueError('unexpected special file: ' + relative)
            result[relative] = entry
    return result

def plan(base, graphics, customized):
    changes = {}
    for name in sorted(base.keys() | graphics.keys()):
        before, after, own = base.get(name), graphics.get(name), customized.get(name)
        if before == after:
            continue
        if own not in (before, after):
            raise ValueError('graphics conflicts with customized asset: ' + name)
        if before and after and before['kind'] != after['kind']:
            raise ValueError('type-changing package update needs review: ' + name)
        # A package path must never traverse a customization's symlink, even
        # when its immediate leaf was absent in both original images.
        for parent in Path(name).parents:
            key = parent.as_posix()
            if key == '.':
                continue
            entry = customized.get(key)
            if entry and entry['kind'] != 'directory':
                raise ValueError('customized parent is not a directory: ' + key)
        if before and before['kind'] == 'directory' and after is None:
            prefix = name + '/'
            if any(key.startswith(prefix) and base.get(key) != entry
                   for key, entry in customized.items()):
                raise ValueError('directory removal would discard custom assets: ' + name)
        changes[name] = {'before': before, 'after': after}
    return changes

def merge(base, graphics, customized, output):
    roots = [Path(p).resolve(strict=True) for p in (base, graphics, customized)]
    output = Path(output).absolute()
    if output.exists() or output.is_symlink():
        raise ValueError('output must not exist')
    parent = output.parent.resolve(strict=True)
    output = parent / output.name
    if any(output == root or output.is_relative_to(root) for root in roots):
        raise ValueError('output may not be inside an input')
    manifests = [inventory(root) for root in roots]
    changes = plan(*manifests)
    shutil.copytree(roots[2], output, symlinks=True)
    # Remove children before parents. Customized descendants were checked above.
    for name, entry in sorted(changes.items(), key=lambda item: -len(Path(item[0]).parts)):
        if entry['after'] is not None:
            continue
        path = output / name
        if path.is_dir() and not path.is_symlink():
            path.rmdir()
        else:
            path.unlink(missing_ok=True)
    for name, entry in sorted(changes.items(), key=lambda item: len(Path(item[0]).parts)):
        after = entry['after']
        if after is None:
            continue
        path = output / name
        if after['kind'] == 'directory':
            path.mkdir(parents=True, exist_ok=True)
            path.chmod(after['mode'])
        else:
            path.unlink(missing_ok=True)
            path.parent.mkdir(parents=True, exist_ok=True)
            if after['kind'] == 'link':
                path.symlink_to(after['target'])
            else:
                shutil.copy2(roots[1] / name, path)
    expected = dict(manifests[2])
    for name, entry in changes.items():
        if entry['after'] is None:
            expected.pop(name, None)
        else:
            expected[name] = entry['after']
    if inventory(output) != expected or any(inventory(root) != manifest for root, manifest in zip(roots, manifests)):
        raise ValueError('input changed during composition or output differs from plan')
    return {'schema': 'shield-graphics-native-composition/1', 'changedPaths': len(changes),
            'changes': changes, 'customizedAssetsPreserved': True, 'inputsUnchanged': True}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('base', 'graphics', 'customized', 'output', 'receipt'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    # Do not overwrite a receipt from an earlier build.
    with args.receipt.open('x') as receipt:
        result = merge(args.base, args.graphics, args.customized, args.output)
        json.dump(result, receipt, indent=2)
        receipt.write('\n')
    print(json.dumps({'ok': True, 'changedPaths': result['changedPaths'], 'receipt': str(args.receipt)}))

if __name__ == '__main__':
    main()
