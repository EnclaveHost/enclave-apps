#!/usr/bin/env python3
"""Compose a memory64 app with a wasm32 proxy of exactly its WASI imports."""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def interfaces(tool, component, direction):
    wit = run(tool, 'component', 'wit', component, capture_output=True, text=True).stdout
    names = re.findall(r'^\s*' + direction + r' ([^;]+);', wit, re.M)
    # Reject inline functions/types and unknown host ABIs rather than letting
    # an unproxied 64-bit canonical pointer reach a 32-bit host binding.
    if any(not re.fullmatch(r'wasi:[\w-]+/[\w-]+@[\d.]+', n) for n in names):
        raise ValueError(f'unsupported {direction} surface: {names}')
    return set(names)


def compose(app, output, w64):
    tool = w64 / 'wasm-tools'
    run(tool, 'validate', '--features', 'all', app)
    wanted = interfaces(tool, app, 'import')
    supported = set(re.findall(r'^\s*export ([^;]+);',
                              (HERE / 'wasiproxy/wit/world.wit').read_text(), re.M))
    supported = {n.split('@')[0] for n in supported}
    unsupported = {n for n in wanted if n.split('@')[0] not in supported
                   or not re.fullmatch(r'0\.2\.\d+', n.split('@')[1])}
    if unsupported:
        raise ValueError(f'WASI interfaces not supported by proxy: {sorted(unsupported)}')
    if not wanted:
        shutil.copyfile(app, output)
        return
    with tempfile.TemporaryDirectory(prefix='w64-proxy-') as tmp:
        root = Path(tmp)
        # Extract the app's exact versioned type graph, including resource
        # dependencies, rather than upgrading its contract to our reference WIT.
        run(tool, 'component', 'wit', app, '--out-dir', root / 'wit')
        for wit in (root / 'wit').glob('*.wit'):
            wit.unlink()
        for name in ('Cargo.toml', 'Cargo.lock'):
            shutil.copyfile(HERE / 'wasiproxy' / name, root / name)
        (root / 'src').mkdir()
        (root / 'wit/world.wit').write_text('package enclave:wasi-proxy;\nworld proxy {\n' +
            ''.join(f'  import {n};\n  export {n};\n' for n in sorted(wanted)) + '}\n')
        resolved = root / 'proxy.json'
        with resolved.open('w') as f:
            run(tool, 'component', 'wit', '--json', root / 'wit', stdout=f)
        with (root / 'src/lib.rs').open('w') as f:
            run(sys.executable, HERE / 'wasiproxy/gen.py', resolved, stdout=f)
        # The unknown-unknown std has no implicit WASI imports. A wasip2 std
        # would pull its own cli/filesystem imports into a minimal guest.
        run('cargo', '+' + os.environ.get('RUST_TC', 'nightly'), 'build',
            '--release', '--locked', '--offline', '--target', 'wasm32-unknown-unknown',
            '--target-dir', root / 'target', cwd=root)
        proxy = root / 'proxy.wasm'
        run(tool, 'component', 'new', root / 'target/wasm32-unknown-unknown/release/wasiproxy.wasm',
            '-o', proxy)
        actual = interfaces(tool, proxy, 'import')
        exported = interfaces(tool, proxy, 'export')
        if actual != wanted or not wanted <= exported:
            raise ValueError(f'proxy surface mismatch: added={sorted(actual - wanted)}, '
                             f'missing={sorted(wanted - actual)}, unhandled={sorted(wanted - exported)}')
        composed = root / 'composed.wasm'
        run(w64 / 'wac', 'plug', '--plug', proxy, app, '-o', composed)
        if interfaces(tool, composed, 'import') != wanted:
            raise ValueError('composition changed the app import surface')
        run(tool, 'validate', '--features', 'all', composed)
        shutil.copyfile(composed, output)
    print(f'[w64] {output}: {len(wanted)} WASI imports, exactly preserved and proxied')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('app', type=Path)
    parser.add_argument('-o', '--output', type=Path, required=True)
    args = parser.parse_args()
    compose(args.app.resolve(), args.output.resolve(),
            Path(os.environ.get('W64', str(Path.home() / '.cache/enclave-w64'))).resolve())
