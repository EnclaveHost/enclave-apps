#!/usr/bin/env python3
"""Convert an ESRGAN/Real-ESRGAN .pth checkpoint to a plain safetensors file
for the sd.cpp upscaler (the upscaler-volume component, generically named
upscaler.safetensors).

Torch-free on purpose: a .pth is a zip holding a pickle (the state dict) plus
raw little-endian storage buffers. The pickle is read with a restricted
Unpickler that recognises ONLY the torch rebuild callables needed for plain
tensors - torch classes never execute, arbitrary GLOBALs are refused - and
the tensors come out as numpy arrays. Output is deterministic (sorted keys,
compact header, no metadata), so the converted file's sha256 is pinnable in
fetch-model.sh as part of the volume recipe.

Wrapper dicts ("params_ema" preferred, then "params") are unwrapped, so the
safetensors carries the bare RRDBNet names (conv_first.*, body.N.*, ...)
sd.cpp's ESRGAN detector reads directly.

Usage: convert_esrgan.py <in.pth> <out.safetensors>
Deps:  numpy
"""
import io
import json
import pickle
import struct
import sys
import zipfile

import numpy as np

# torch storage class name -> numpy dtype (fp32 checkpoints in practice; the
# rest listed for completeness)
STORAGE_DTYPES = {
    "FloatStorage": np.dtype("<f4"),
    "HalfStorage": np.dtype("<f2"),
    "DoubleStorage": np.dtype("<f8"),
    "IntStorage": np.dtype("<i4"),
    "LongStorage": np.dtype("<i8"),
}

SAFE_DTYPES = {"<f4": "F32", "<f2": "F16", "<f8": "F64", "<i4": "I32", "<i8": "I64"}


class StorageRef:
    def __init__(self, dtype, key):
        self.dtype, self.key = dtype, key


class TensorStub:
    def __init__(self, ref, offset, size, stride):
        self.ref, self.offset, self.size, self.stride = ref, offset, size, stride


def _rebuild_tensor(storage, storage_offset, size, stride, *_):
    return TensorStub(storage, storage_offset, tuple(size), tuple(stride))


class EsrganUnpickler(pickle.Unpickler):
    """Only what a plain tensor state dict needs; everything else refuses."""

    def find_class(self, module, name):
        if module == "torch._utils" and name in ("_rebuild_tensor_v2", "_rebuild_tensor"):
            return _rebuild_tensor
        if module == "torch" and name in STORAGE_DTYPES:
            return name  # a marker consumed by persistent_load
        if module == "collections" and name == "OrderedDict":
            import collections

            return collections.OrderedDict  # stdlib; carries _metadata via BUILD
        raise pickle.UnpicklingError(f"refusing pickle global {module}.{name}")

    def persistent_load(self, pid):
        # ('storage', <StorageType marker>, key, location, numel)
        if not (isinstance(pid, tuple) and len(pid) >= 3 and pid[0] == "storage"):
            raise pickle.UnpicklingError(f"unexpected persistent id {pid!r}")
        return StorageRef(STORAGE_DTYPES[pid[1]], pid[2])


def load_pth(path):
    with zipfile.ZipFile(path) as z:
        pkl = next(n for n in z.namelist() if n.endswith("/data.pkl") or n == "data.pkl")
        root = pkl[: -len("data.pkl")]
        obj = EsrganUnpickler(io.BytesIO(z.read(pkl))).load()
        # unwrap {"params_ema": {...}} / {"params": {...}} training containers
        for wrapper in ("params_ema", "params"):
            if isinstance(obj, dict) and isinstance(obj.get(wrapper), dict):
                obj = obj[wrapper]
                break
        tensors = {}
        for name, t in obj.items():
            if not isinstance(t, TensorStub):
                continue  # non-tensor bookkeeping entries
            n = int(np.prod(t.size)) if t.size else 1
            # contiguous C-order only - true for every conv/bias here
            expect, ok = 1, True
            for d, s in zip(reversed(t.size), reversed(t.stride)):
                ok = ok and (d == 1 or s == expect)
                expect *= d
            if not ok:
                raise SystemExit(f"{name}: non-contiguous tensor (stride {t.stride})")
            raw = z.read(f"{root}data/{t.ref.key}")
            a = np.frombuffer(raw, dtype=t.ref.dtype, count=n, offset=t.offset * t.ref.dtype.itemsize)
            tensors[name] = a.reshape(t.size)
        return tensors


def write_safetensors(tensors, path):
    header, offset = {}, 0
    names = sorted(tensors)
    for name in names:
        a = tensors[name]
        nbytes = a.nbytes
        header[name] = {
            "dtype": SAFE_DTYPES[a.dtype.str],
            "shape": list(a.shape),
            "data_offsets": [offset, offset + nbytes],
        }
        offset += nbytes
    hjson = json.dumps(header, sort_keys=True, separators=(",", ":")).encode()
    with open(path, "wb") as f:
        f.write(struct.pack("<Q", len(hjson)))
        f.write(hjson)
        for name in names:
            f.write(tensors[name].tobytes())


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    tensors = load_pth(sys.argv[1])
    if "conv_first.weight" not in tensors:
        raise SystemExit(
            f"no conv_first.weight among {len(tensors)} tensors - not an "
            "RRDBNet/ESRGAN state dict (sd.cpp's upscaler runs only this arch)"
        )
    blocks = len({n.split(".")[1] for n in tensors if n.startswith("body.")})
    scale = 4 if "conv_up2.weight" in tensors else (2 if "conv_up1.weight" in tensors else 1)
    write_safetensors(tensors, sys.argv[2])
    print(f"{sys.argv[2]}: {len(tensors)} tensors, {blocks} RRDB blocks, {scale}x")


if __name__ == "__main__":
    main()
