#!/usr/bin/env python3
"""Check actual packaged public GGUF tensors through the experimental Q8 gate.

This is a host-only probe. It neither launches OpenCL nor contacts a GPU.
GGUFReader independently parses the approved model and extracts raw block bytes.
"""
import argparse
import hashlib
import json
from pathlib import Path
import time

from gguf import GGUFReader
from q8_admission import BufferView, PublicQ8Catalog, validate_launch


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--packaged', type=Path, required=True)
    parser.add_argument('--models', type=Path, required=True)
    parser.add_argument('--engine', choices=('llama', 'sd'), default='llama')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    started = time.monotonic()
    spec = json.loads((args.packaged / 'service.json').read_text())['models'][args.engine]
    model = args.models / Path(spec['model']).name
    if sha(model) != spec['modelSha256']:
        raise ValueError('packaged model digest mismatch')
    policy = args.packaged / 'policy' / Path(spec['policy']).name
    calibration = args.packaged / 'policy' / Path(spec['calibration']).name
    calibration_sha = sha(calibration)
    approved = PublicQ8Catalog(policy.read_bytes(), spec['policySha256'],
                               spec['modelSha256'], calibration_sha)
    reader = GGUFReader(str(model))
    rows, skipped = [], []
    negative_checks = 0
    for tensor in reader.tensors:
        if int(tensor.tensor_type) != 8:
            continue
        shape = [int(x) for x in tensor.shape]
        shape += [1] * (4-len(shape))
        k, n, batch, outer = shape
        if batch != 1 or outer != 1:
            skipped.append({'name': tensor.name, 'shape': shape, 'reason': 'batched tensor'})
            continue
        # Same byte layout as upstream's convert_block_q8_0. This probe uses
        # independent GGUFReader extraction, not that kernel or our hash loop.
        packed = tensor.data.view('uint8').reshape(-1, 34)
        quants, scales = packed[:, 2:].tobytes(), packed[:, :2].tobytes()
        name = tensor.name
        # Some SD policies use externally prefixed tensor names. Infer nothing:
        # this probe requires the exact name emitted by its approved GGUF.
        authenticated = approved.authenticate(name, quants, scales, k, n)
        layouts, declined_layouts = [], []
        for variant, m in [('mm', 32), ('mv', 1)]:
            scalars = {3: 0, 5: 0, 6: k, 7: n, 17: 1, 18: 1}
            if variant == 'mm':
                scalars.update({8: 1, 9: m, 10: 1, 11: k, 12: k, 13: n,
                                14: k*n, 15: k*m, 16: n*m})
                grid, local = ((n+63)//64*128, (m+63)//64, 1), (128, 1, 1)
            else:
                row = k//32*34
                scalars.update({8: row, 9: row*n, 10: row*n, 11: 1, 12: k*4,
                                13: m*k*4, 14: m*k*4, 15: n, 16: m})
                grid, local = ((n+7)//8*16, m*2, 1), (16, 2, 1)
            sizes = {0: k*n, 1: k*n//16, 2: k*m*4, 4: n*m*4}
            buffers = {i: BufferView(i+1, size, 0, size) for i, size in sizes.items()}
            try:
                validate_launch(variant, scalars, buffers, grid, local)
            except ValueError as error:
                declined_layouts.append({'variant': variant, 'reason': str(error)})
            else:
                layouts.append(variant)
        # Mutate one code and one scale after an earlier successful admission.
        # Neither the tensor name nor prior success may bypass a new hash.
        if not rows:
            for q, d in [(bytes([quants[0] ^ 1])+quants[1:], scales),
                         (quants, bytes([scales[0] ^ 1])+scales[1:])]:
                try:
                    approved.authenticate(name, q, d, k, n)
                except ValueError:
                    negative_checks += 1
                else:
                    raise AssertionError('mutated model bytes admitted')
        rows.append({'name': name, 'shape': shape, 'packedBytes': int(tensor.n_bytes),
                     'sha256': authenticated.sha256, 'validatedLayouts': layouts,
                     'declinedLayouts': declined_layouts})
        del quants, scales, authenticated
    receipt = {'schema': 'shield-q8-admission-probe/1', 'engine': args.engine,
               'modelSha256': spec['modelSha256'], 'policySha256': spec['policySha256'],
               'calibrationSha256': calibration_sha, 'tensors': rows, 'skipped': skipped,
               'authenticatedTensorCount': len(rows), 'mutationRejections': negative_checks,
               'validatedLaunchCount': sum(len(row['validatedLayouts']) for row in rows),
               'declinedLaunchCount': sum(len(row['declinedLayouts']) for row in rows),
               'elapsedSeconds': round(time.monotonic()-started, 3),
               'maskedDispatch': False, 'openclExecution': False,
               'scope': 'actual model bytes; synthetic launch metadata from upstream ABI'}
    if not rows or negative_checks != 2:
        raise AssertionError('probe did not exercise tensor admission and mutation checks')
    args.output.write_text(json.dumps(receipt, indent=2)+'\n')
    print(json.dumps({key: value for key, value in receipt.items() if key not in ('tensors', 'skipped')}))


if __name__ == '__main__':
    main()
