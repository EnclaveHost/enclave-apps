#!/usr/bin/python3
"""Call the local Shield service; no direct GPU or worker connections."""
import argparse
import base64
import json
import os
from pathlib import Path
import socket
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from service import receive, send, MAX_RESPONSE, FIXED_ARITHMETIC, NATIVE_ARITHMETIC


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('engine', choices=['status', 'llama', 'sd', 'graph'])
    p.add_argument('-p', '--prompt')
    p.add_argument('-f', '--file', type=Path)
    p.add_argument('-o', '--output', type=Path)
    precision = p.add_mutually_exclusive_group()
    precision.add_argument('--cpu', action='store_true', help='use native model arithmetic inside the VM')
    precision.add_argument('--approximate', action='store_true',
        help='opt into calibrated fixed-point masked GPU arithmetic; output may differ from CPU')
    p.add_argument('--seed', type=int, default=42)
    p.add_argument('--tokens', type=int, default=128)
    a = p.parse_args()
    if a.approximate and a.engine not in ('llama', 'sd'):
        p.error('--approximate is only for native llama/sd; graph arithmetic is exact')
    request = {'op': a.engine}
    if a.engine == 'graph':
        if not a.file: p.error('graph needs -f request.json containing graph and inputs')
        value = json.loads(a.file.read_text())
        request.update(graph=value['graph'], inputs=value['inputs'], mode='cpu' if a.cpu else 'masked')
    elif a.engine != 'status':
        if not a.cpu and not a.approximate:
            p.error('choose --cpu or --approximate; native masked arithmetic can change the output')
        prompt = a.file.read_text() if a.file else a.prompt
        if prompt is None: p.error('supply --prompt or --file')
        request.update(prompt=prompt, mode='cpu' if a.cpu else 'masked', seed=a.seed)
        request['arithmetic'] = NATIVE_ARITHMETIC if a.cpu else FIXED_ARITHMETIC
        if a.engine == 'llama': request['tokens'] = a.tokens
        if a.engine == 'sd' and a.output is None: p.error('sd needs --output image.png')
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(1800)
        sock.connect('/run/shield/compute.sock')
        send(sock, request)
        result = receive(sock, MAX_RESPONSE)
    if not result.get('ok'):
        p.exit(1, result.get('error', 'compute refused') + '\n')
    if 'png' in result:
        # Never overwrite an existing file or follow a symlink implicitly.
        with os.fdopen(os.open(a.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb') as f:
            f.write(base64.b64decode(result.pop('png'), validate=True))
        print(a.output)
    elif 'text' in result:
        print(result.pop('text'))
    # Status/graph JSON is the command's result and should compose with tools
    # such as jq. Native text/image commands keep diagnostics separate.
    print(json.dumps(result), file=sys.stdout if a.engine in ('status', 'graph') else sys.stderr)


if __name__ == '__main__':
    main()
