"""Independent EVMSNAP1 fixture, using Python cryptography (not Rust code)."""
import hashlib
import hmac
import json
from pathlib import Path
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

def fields(*values):
    return b''.join(len(v.encode()).to_bytes(8, 'big') + v.encode() for v in values)

scope = hashlib.sha256(b'enclave-private-snapshot-v1' + fields('risc-box', 'alice', 'deployment')).digest()
context = hashlib.sha256(scope + fields('bucket', 'session')).digest()
header = b'EVMSNAP1' + bytes(range(32))
key = hmac.new(bytes([42]) * 32, context + header, hashlib.sha256).digest()
plain = b'private RAM'
sealed = header
for index, part in enumerate((plain, b'')):
    size = len(part).to_bytes(4, 'big')
    counter = index.to_bytes(8, 'big')
    sealed += size + AESGCM(key).encrypt(b'\x00' * 4 + counter, part, context + header + counter + size)
path = Path(__file__).parent / 'fixtures/reference.json'
path.write_text(json.dumps({'plaintextHex': plain.hex(), 'envelopeHex': sealed.hex()}, indent=2) + '\n')
print('Wrote independent AES-GCM fixture (synthetic test key only).')
