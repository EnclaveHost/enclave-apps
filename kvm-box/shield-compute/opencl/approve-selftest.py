"""Image-build-only approval of a deterministic, non-secret matrix fixture."""
import json
from pathlib import Path
import sys
import numpy as np
sys.path.insert(0, '/shield')
from compiler import constant_digest

path = Path('/shield/service.json')
config = json.loads(path.read_text())
public_fixture = (np.arange(32 * 16, dtype=np.int64) % 5 - 2).reshape(32, 16)
digest = constant_digest(public_fixture)
config['publicConstants'] = sorted(set(config['publicConstants']) | {digest})
path.write_text(json.dumps(config, indent=2) + '\n')
