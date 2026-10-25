#!/usr/bin/env python3
"""Public-fixture numerical comparisons inside an isolated candidate container.

Mount public models at /models, the compiled evaluate-logits at /evidence,
and this script at /tests. Only the dedicated host worker 9741 is used.
These descriptive measurements are not general model-quality acceptance.
"""
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import subprocess
import sys
import time

import numpy as np

ROOT = Path('/evidence')
TEXTS = {
    'science': 'A copper wire carries an electric current when a voltage is applied across it. '
        'The moving electrons transfer energy to the material through collisions. Increasing the '
        'wire length increases its resistance, while increasing its cross-sectional area reduces '
        'resistance. A practical circuit must account for heating and the limits of its power supply.',
    'reasoning': 'Mira puts three blue marbles and five green marbles into a bag. She adds two more '
        'blue marbles, then removes one green marble. The bag now contains five blue marbles and '
        'four green marbles, for a total of nine. Counting the two colors separately makes it easier '
        'to check each operation before calculating the final total.',
    'code': 'The Python function def square(x): return x * x maps a number to its square. '
        'For inputs [-3, 0, 4], the corresponding outputs are [9, 0, 16]. A unit test should '
        'include negative numbers and zero. A loop can apply the function to each list element '
        'without modifying the original list.',
    'multilingual': 'The library opens at nine in the morning. La biblioteca abre a las nueve de '
        'la mañana. Die Bibliothek öffnet um neun Uhr morgens. Readers can borrow books, use a '
        'quiet study room, and ask the librarian for help. Please return borrowed books on time '
        'so that other readers can enjoy them too.',
}
IMAGES = [
    ('portrait', 'A studio portrait of a woman wearing a blue knitted hat, soft window light, detailed eyes', 17),
    ('geometry', 'A red cube beside a yellow sphere on a plain white table, clean geometric shapes, soft shadows', 29),
    ('landscape', 'A snowy mountain reflected in a still lake at sunset, pine forest, detailed landscape photograph', 53),
    ('typography', 'A simple poster with the word HELLO in bold black letters on a white background', 71),
]


def logits(path):
    with path.open('rb') as file:
        count, width = struct.unpack('<ii', file.read(8))
        assert 1 < count <= 384 and 0 < width <= 200000
        tokens = np.fromfile(file, dtype='<i4', count=count)
        values = np.fromfile(file, dtype='<f4').reshape(count, width)
    assert len(tokens) == count and np.isfinite(values).all()
    return tokens, values


def compare_logits(cpu, masked):
    tokens, a = logits(cpu)
    other, b = logits(masked)
    assert np.array_equal(tokens, other) and a.shape == b.shape
    rows = []
    for i in range(len(tokens) - 1):
        x, y = a[i].astype('float64'), b[i].astype('float64')
        x -= x.max(); y -= y.max()
        px, py = x - np.log(np.exp(x).sum()), y - np.log(np.exp(y).sum())
        target = tokens[i + 1]
        rows.append({'cpuNll': -float(px[target]), 'maskedNll': -float(py[target]),
            'kl': float(np.sum(np.exp(px) * (px - py))),
            'top1Same': bool(np.argmax(x) == np.argmax(y)),
            'top5Overlap': len(set(np.argpartition(x, -5)[-5:]) & set(np.argpartition(y, -5)[-5:])) / 5})
    cpu_nll = np.mean([r['cpuNll'] for r in rows])
    masked_nll = np.mean([r['maskedNll'] for r in rows])
    return {'tokens': len(tokens), 'predictedPositions': len(rows),
        'cpuMeanNll': float(cpu_nll), 'maskedMeanNll': float(masked_nll),
        'cpuPerplexity': float(np.exp(cpu_nll)), 'maskedPerplexity': float(np.exp(masked_nll)),
        'perplexityRatio': float(np.exp(masked_nll - cpu_nll)),
        'meanKlCpuToMasked': float(np.mean([r['kl'] for r in rows])),
        'top1Agreement': float(np.mean([r['top1Same'] for r in rows])),
        'meanTop5Overlap': float(np.mean([r['top5Overlap'] for r in rows]))}


def main():
    spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
    service = importlib.util.module_from_spec(spec); spec.loader.exec_module(service)
    config = json.loads(Path('/shield/service.json').read_text())
    config['worker'] = {'host': '127.0.0.1', 'port': 9741}; config['jobs'] = '/tmp'
    compute = service.Compute(config)
    receipt = {'schema': 'shield-native-numerics/1',
        'scope': 'authored public diagnostic fixtures; no general quality or speedup claim',
        'modelPolicies': {k: v['policySha256'] for k, v in config['models'].items()},
        'llama': [], 'sd': []}
    def save():
        (ROOT/'numerics.json').write_text(json.dumps(receipt, indent=2) + '\n')
    for name, text in TEXTS.items():
        prompt = ROOT/(name+'.txt'); prompt.write_text(text)
        record = {'name': name, 'text': text, 'runs': {}}
        for mode in ('cpu', 'masked'):
            path = ROOT/(name+'-'+mode+'.logits')
            env = compute.environment(config['models']['llama'], mode == 'masked', '/tmp', 'llama')
            start = time.monotonic()
            process = subprocess.run(['/evidence/evaluate-logits', config['models']['llama']['model'],
                str(prompt), str(path)], env=env, capture_output=True, text=True, timeout=180)
            (ROOT/(name+'-'+mode+'.stderr')).write_text(process.stderr)
            assert process.returncode == 0, (name, mode, process.returncode)
            stats = service.parse_native_stats(process.stderr)
            if mode == 'masked':
                assert stats and stats['exchanges'] > 0 and stats['verification_failures'] == 0, stats
            record['runs'][mode] = {'seconds': time.monotonic()-start, 'stats': stats,
                                   'logitsSha256': hashlib.sha256(path.read_bytes()).hexdigest()}
        record['comparison'] = compare_logits(ROOT/(name+'-cpu.logits'), ROOT/(name+'-masked.logits'))
        receipt['llama'].append(record); save()
        print(name+': '+json.dumps(record['comparison']), flush=True)
        for mode in ('cpu', 'masked'): (ROOT/(name+'-'+mode+'.logits')).unlink()
    for name, prompt, seed in IMAGES:
        record = {'name': name, 'prompt': prompt, 'seed': seed, 'runs': {}}
        for mode in ('cpu', 'masked'):
            start = time.monotonic()
            request = {'op': 'sd', 'mode': mode, 'prompt': prompt, 'seed': seed}
            # Explicit for service revisions that enforce a numerical contract.
            if hasattr(service, 'FIXED_ARITHMETIC'):
                request['arithmetic'] = service.FIXED_ARITHMETIC if mode == 'masked' else service.NATIVE_ARITHMETIC
            result = compute.run(request)
            png = base64.b64decode(result.pop('png')); (ROOT/(name+'-'+mode+'.png')).write_bytes(png)
            if mode == 'masked': assert result['stats']['exchanges'] > 0 and result['stats']['verification_failures'] == 0
            record['runs'][mode] = {'seconds': time.monotonic()-start, 'stats': result['stats'],
                                   'pngSha256': hashlib.sha256(png).hexdigest()}
        receipt['sd'].append(record); save()
        print(name+': image pair generated', flush=True)
    receipt['generationComplete'] = True; save()


def finish_images(directory):
    # Pillow is a host analysis dependency, not a new guest runtime dependency.
    from PIL import Image
    receipt = json.loads((directory/'numerics.json').read_text())
    assert receipt['generationComplete'] and len(receipt['sd']) == len(IMAGES)
    for record in receipt['sd']:
        name = record['name']
        a = np.array(Image.open(directory/(name+'-cpu.png')).convert('RGB'), dtype='float64')
        b = np.array(Image.open(directory/(name+'-masked.png')).convert('RGB'), dtype='float64')
        assert a.shape == b.shape == (256, 256, 3)
        mse = float(np.mean((a-b)**2))
        record['comparison'] = {'mae': float(np.mean(abs(a-b))),
            'psnrDB': float(10*np.log10(255**2/mse)) if mse else None,
            'pixelExact': mse == 0, 'maximumChannelDifference': int(np.max(abs(a-b)))}
        print(name+': '+json.dumps(record['comparison']), flush=True)
    receipt['complete'] = True
    temporary = directory/'numerics-analyzed.json'
    temporary.write_text(json.dumps(receipt, indent=2)+'\n')
    temporary.replace(directory/'numerics.json')


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--finish-images':
        finish_images(Path(sys.argv[2]))
    else:
        assert len(sys.argv) == 1
        main()
