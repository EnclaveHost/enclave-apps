#!/usr/bin/env python3
"""Create validated equivalent and unsupported-control-flow shader fixtures."""
from pathlib import Path
import argparse
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
source = (Path(__file__).parent / 'linear-prepass.comp').read_text()
variants = {
    'equivalent': source.replace('positions', 'transformed').replace('row', 'item').replace('source', 'input_data'),
    'unmatched': source.replace('    destination.positions[row] =', '    if (row < 64u)\n    destination.positions[row] ='),
}
for name, shader in variants.items():
    output = args.output / ('build-' + name)
    output.mkdir()
    for filename in ['prepass.vert.spv', 'prepass.frag.spv']:
        shutil.copy2(args.output / 'build' / filename, output / filename)
    path = output / 'linear-prepass.comp'
    path.write_text(shader)
    compiled = output / 'linear-prepass.comp.spv'
    subprocess.run(['glslangValidator', '-V', '--target-env', 'vulkan1.1', str(path), '-o', str(compiled)], check=True)
    subprocess.run(['spirv-val', '--target-env', 'vulkan1.1', str(compiled)], check=True)
