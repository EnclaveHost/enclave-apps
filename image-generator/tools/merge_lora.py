#!/usr/bin/env python3
"""Merge a Lightning LoRA into the Qwen-Image-2512 transformer and emit ONE
bf16 safetensors file ready for sd.cpp conversion.

Streams tensor-by-tensor with a hand-rolled safetensors writer (8-byte
header length + JSON header + raw data), so peak RAM stays near the largest
single tensor (~100 MB) despite the ~41 GB model. LoRA key conventions
handled: ComfyUI ("diffusion_model.<path>.lora_{A,B}.weight" /
".lora_{down,up}.weight") and diffusers ("transformer.<path>..."), with
optional ".alpha" scaling (alpha absent = alpha == rank, i.e. scale 1).

Usage: merge_lora.py <base_dir_or_file> <lora.safetensors> <out.safetensors>
Deps:  pip install torch safetensors numpy  (CPU torch is fine)
"""
import json
import pathlib
import sys

import torch
from safetensors import safe_open

DTYPES = {
    torch.bfloat16: ("BF16", 2, torch.int16),
    torch.float16: ("F16", 2, torch.int16),
    torch.float32: ("F32", 4, torch.int32),
}


def lora_pairs(lora_path):
    """{target tensor name: (down, up, scale)} from a LoRA safetensors."""
    pairs = {}
    with safe_open(lora_path, framework="pt", device="cpu") as f:
        keys = list(f.keys())
        downs = {}
        for k in keys:
            for marker in (".lora_A.weight", ".lora_down.weight"):
                if k.endswith(marker):
                    downs[k[: -len(marker)]] = k
        for stem, dk in downs.items():
            uk = next(
                (stem + m for m in (".lora_B.weight", ".lora_up.weight") if stem + m in keys),
                None,
            )
            if uk is None:
                print(f"WARN: no up-projection for {stem}, skipped", file=sys.stderr)
                continue
            down = f.get_tensor(dk).float()
            up = f.get_tensor(uk).float()
            scale = 1.0
            if stem + ".alpha" in keys:
                scale = float(f.get_tensor(stem + ".alpha").float()) / down.shape[0]
            # strip the framework prefix down to the transformer-local path
            target = stem
            for prefix in ("diffusion_model.", "transformer."):
                if target.startswith(prefix):
                    target = target[len(prefix):]
                    break
            pairs[target + ".weight"] = (down, up, scale)
    return pairs


def base_shards(base):
    """[(shard path, [tensor names])] for a single file or a diffusers dir."""
    p = pathlib.Path(base)
    if p.is_file():
        with safe_open(p, framework="pt", device="cpu") as f:
            return [(p, list(f.keys()))]
    idx = p / "diffusion_pytorch_model.safetensors.index.json"
    if idx.is_file():
        by_shard = {}
        for name, shard in json.loads(idx.read_text())["weight_map"].items():
            by_shard.setdefault(p / shard, []).append(name)
        return sorted((s, sorted(n)) for s, n in by_shard.items())
    shards = sorted(p.glob("*.safetensors"))
    if not shards:
        sys.exit(f"no safetensors under {base}")
    out = []
    for s in shards:
        with safe_open(s, framework="pt", device="cpu") as f:
            out.append((s, list(f.keys())))
    return out


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    base, lora, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
    pairs = lora_pairs(lora)
    print(f"LoRA targets {len(pairs)} tensors")
    shards = base_shards(base)

    # pass 1: shapes/dtypes -> header with data offsets
    metas = []  # (shard, name, out_name, dtype, shape, nbytes)
    for shard, names in shards:
        with safe_open(shard, framework="pt", device="cpu") as f:
            for name in names:
                sl = f.get_slice(name)
                shape = sl.get_shape()
                dt = str(sl.get_dtype()).replace("torch.", "").upper()
                dtype = {"BFLOAT16": torch.bfloat16, "BF16": torch.bfloat16,
                         "FLOAT16": torch.float16, "F16": torch.float16,
                         "FLOAT32": torch.float32, "F32": torch.float32}.get(dt)
                if dtype is None:
                    sys.exit(f"{name}: unhandled dtype {dt}")
                n = 1
                for d in shape:
                    n *= d
                out_name = name[len("transformer."):] if name.startswith("transformer.") else name
                metas.append((shard, name, out_name, dtype, shape, n * DTYPES[dtype][1]))
    header, off = {}, 0
    for _, _, out_name, dtype, shape, nbytes in metas:
        header[out_name] = {"dtype": DTYPES[dtype][0], "shape": list(shape),
                            "data_offsets": [off, off + nbytes]}
        off += nbytes
    hdr = json.dumps(header, separators=(",", ":")).encode()
    hdr += b" " * (-(len(hdr)) % 8)  # 8-byte alignment keeps mmap readers happy

    # pass 2: stream tensors, merging where the LoRA hits
    hit = 0
    todo = dict(pairs)
    with open(out_path, "wb") as w:
        w.write(len(hdr).to_bytes(8, "little"))
        w.write(hdr)
        for shard, names in shards:
            print(f"shard {shard.name}: {len(names)} tensors")
            with safe_open(shard, framework="pt", device="cpu") as f:
                for name in names:
                    t = f.get_tensor(name)
                    out_name = name[len("transformer."):] if name.startswith("transformer.") else name
                    if out_name in todo:
                        down, up, scale = todo.pop(out_name)
                        t = (t.float() + scale * (up @ down)).to(t.dtype)
                        hit += 1
                    view_dtype = DTYPES[t.dtype][2]
                    w.write(t.contiguous().view(view_dtype).numpy().tobytes())
                    del t
    if todo:
        print(f"ERROR: {len(todo)} LoRA tensors matched nothing, e.g. "
              f"{sorted(todo)[:3]}", file=sys.stderr)
        sys.exit("the merge is incomplete - key mapping needs attention (output NOT trustworthy)")
    print(f"merged {hit}/{len(pairs)} LoRA targets into {len(metas)} tensors -> {out_path}")


if __name__ == "__main__":
    main()
