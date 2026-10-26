#!/usr/bin/env python3
"""Experimental frontend helper: typed recognition and launch admission only.

Run with Python -I from an immutable package path. This helper never enables
dispatch and never receives tensor contents. Do not install as the default
compiler until the complete Q8 execution contract is implemented.
"""
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from kernel_compiler import compile_source, compile_modules, MAX_IL
from q8_kernel_compiler import analyze
from q8_admission import BufferView, validate_launch


def main():
    try:
        request = json.loads(sys.stdin.buffer.read(2*MAX_IL+4096))
        if set(request) == {'source', 'options'}:
            result = (analyze(request['source'], request['options'])
                      if '-cl-std=CL3.0' in request['options'].split()
                      else compile_source(request['source'], request['options']))
        elif set(request) == {'modules'}:
            result = compile_modules(request['modules'])
        elif set(request) == {'q8Launch'}:
            value = request['q8Launch']
            scalars = {int(key): number for key, number in value['scalars'].items()}
            buffers = {int(key): BufferView(**buffer) for key, buffer in value['buffers'].items()}
            launch = validate_launch(value['variant'], scalars, buffers, value['global'],
                                     value['local'], value['offset'])
            result = {'schema': 'shield-q8-capture-layout/1', 'dispatchable': False,
                      'm': launch.m, 'n': launch.n, 'k': launch.k,
                      'readBytes': [end-start for _, start, end in launch.reads],
                      'outputBytes': launch.write[2]-launch.write[1]}
        else:
            result = {}
    except Exception:
        # A failed audit helper can only decline capture; it cannot grant work.
        result = {}
    print(json.dumps(result, separators=(',', ':')))


if __name__ == '__main__':
    main()
