#!/usr/bin/env python3
"""Compare a transport-only rebuild with the pinned deployed core.

This checks encoding regression on public synthetic inputs, not model quality
or masked GPU execution. Arguments: baseline.so candidate.so.
"""
import ctypes as C
import json
import random
import struct
import sys

libs = [C.CDLL(p) for p in sys.argv[1:]]
assert len(libs) == 2
for lib in libs:
    lib.sh_encode_weight_fixed.argtypes = [C.c_uint16,C.c_int8]
    lib.sh_encode_weight_fixed.restype = C.c_int64
    lib.sh_float_to_half.argtypes = [C.c_float]
    lib.sh_float_to_half.restype = C.c_uint16
    lib.sh_prepare_weight.argtypes = [C.POINTER(C.c_uint16),C.POINTER(C.c_int8),
                                     C.c_int64,C.c_int64,C.POINTER(C.c_uint16),C.POINTER(C.c_int)]
    lib.sh_prepare_weight.restype = C.c_int
encodings = 0
for half in range(65536):
    if half & 0x7c00 == 0x7c00: continue  # finite fp16 scales only
    for quant in [-119,-1,0,1,119]:
        assert libs[0].sh_encode_weight_fixed(half,quant) == libs[1].sh_encode_weight_fixed(half,quant)
        encodings += 1
rng = random.Random(2048)
for _ in range(65536):
    value, = struct.unpack('<f',struct.pack('<I',rng.getrandbits(32)))
    assert libs[0].sh_float_to_half(value) == libs[1].sh_float_to_half(value)
prepared = 0
for k in [32,64,128]:
    for n in [1,8,16,32]:
        scales = (C.c_uint16*(k//32*n))(*[struct.unpack('<H',struct.pack('<e',rng.uniform(.0001,.03)))[0]
                                             for _ in range(k//32*n)])
        weights = (C.c_int8*(k*n))(*[rng.randint(-119,119) for _ in range(k*n)])
        results = []
        for lib in libs:
            scaled = (C.c_uint16*(k//32*n))(); exponents = (C.c_int*n)()
            code = lib.sh_prepare_weight(scales,weights,k,n,scaled,exponents)
            results.append((code,bytes(scaled),bytes(exponents)))
        assert results[0] == results[1] and results[0][0] >= 0
        prepared += 1
print(json.dumps({'schema':'native-core-encoding-regression/1','weightEncodings':encodings,
                  'floatToHalf':65536,'preparedWeightMatrices':prepared,'identical':True,
                  'syntheticInputs':True,'modelQualityEstablished':False,'gpuWork':False}))
