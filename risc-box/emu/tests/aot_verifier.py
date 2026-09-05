#!/usr/bin/env python3
"""Check AOT verification against two tiny regions and real Sv39 mappings.

The production corpus is large and unrelated to these cache invariants. Build
an isolated copy of the emulator with two one-instruction regions, and append
a probe inside cpu.rs so it can exercise the actual private verifier. No test
hooks or fixture code are linked into the shipped emulator.
"""
from pathlib import Path
import shutil
import subprocess
import tempfile

source = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='rbx-aot-verifier-') as scratch:
    crate = Path(scratch) / 'emu'
    shutil.copytree(source, crate, ignore=shutil.ignore_patterns('target'))
    (crate / 'aot/regions.dump').write_text(
        'REGION 1 1\nB 80000000 1\nO 1 1048723 0 7 1 0 0 4\n'
        'REGION 2 1\nB ffffffc000000000 1\nO 1 1048723 0 7 1 0 0 4\n'
    )
    cpu = crate / 'src/cpu.rs'
    cpu.write_text(cpu.read_text() + '\n' + (source / 'tests/fixtures/aot_verifier_probe.rs').read_text())
    (crate / 'examples/aot-verifier-regression.rs').write_text(
        'extern crate riscv_emu_rust; fn main() { '
        'riscv_emu_rust::cpu::aot_verifier_regression_probe(); }\n'
    )
    subprocess.run(['cargo', '+nightly', 'run', '--release', '--manifest-path',
                    str(crate / 'Cargo.toml'), '--example',
                    'aot-verifier-regression', '--features', 'aot'], check=True)
